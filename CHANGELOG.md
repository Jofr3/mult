# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and the project aims to adhere to
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

Everything since the initial prototype, covering the thirteen slices of
[docs/PLAN.md](docs/PLAN.md) — the test harness, the daemon hot path, client PTY
integrity, the event loop, security hardening, rendering, the CLI, interaction
affordances, concurrency, two architecture passes, docs/CI/release, and a final
cleanup pass. The wire protocol moved to `PROTOCOL_VERSION` 11 over that span; a client and daemon from
different points in it will not interoperate and must be restarted together.

### Migrations

Both are round-trip tested, and both are invisible in normal use.

- A pre-11a `state.json` still decodes its `status: "Running"|"Stopped"`
  terminal field; `Running` folds into `restore_on_launch: true` at load, and
  the next save writes only the new key.
- A pre-11a `state.json` still decodes chat `messages`. Because this build has
  nowhere to show them, storage copies the file to
  `state.json.pre-11a-<timestamp>-<random>` before anything can save over it and
  names the copy on the status line, so the data survives the save that drops
  it. A state file without transcripts is neither copied nor mentioned.

### Added

- Prebuilt release archives (H5). A tag-triggered
  `.github/workflows/release.yml` builds `x86_64` Linux (gnu and musl) and macOS
  (`x86_64` and `aarch64`) archives and publishes them to a GitHub Release with
  a `SHA256SUMS` file. Every archive contains **both** `mult` and `mult-server`,
  because the client autospawns the daemon from a path adjacent to its own
  binary and ownership-checks it before executing. The README documents
  `cargo install --git`, `nix run` and the archives.
- `docs/CONFIG.md` (H7): a complete configuration reference. All twelve
  `colorscheme` keys are now documented with what each one actually colours —
  including `_nc`, whose leading underscore nobody would guess — alongside the
  read-time file requirements and the validation policy.
- `docs/TROUBLESHOOTING.md` (H8): the failure modes this code produces, each
  with the exact message text the user sees — protocol mismatch after an
  upgrade, autospawn refused by the daemon-binary ownership check, `.corrupt-*`
  and `.pre-11a-*` state backups, the client/session caps, peer-credential
  failures, panes retired on daemon loss, a missing `pi`/`claude`, and status
  reporting disabled on a non-private runtime directory.
- `docs/RELEASING.md` (H17): a release checklist covering the version pins, the
  CHANGELOG and the tag.
- Fuzz targets (G3) in a separate `fuzz/` workspace, kept out of the main one so
  they do not affect normal builds or `cargo deny`: `protocol_read_message`
  drives `read_message` over arbitrary bytes as both wire types, and
  `vt_response_detector` drives the terminal-response state machine over
  arbitrary PTY output. CI builds both and runs a time-boxed smoke pass; real
  campaigns are manual.
- Coverage measurement (H9): `just coverage`, `just coverage-html`, and a CI job
  running `cargo llvm-cov`. Line coverage is **87.5%** across the workspace. The
  CI floor is set below that as a regression guard, not a target.
- Issue and PR templates and a `CODEOWNERS` file (H11). The bug report asks for
  `mult --version`, terminal emulator, `$TERM`, OS, and whether the kitty
  keyboard protocol is active — all of which decide how input is encoded.
- `just install-hooks` (H16), writing a `pre-commit` hook that runs only
  `cargo fmt --all -- --check`, deliberately cheap enough not to be bypassed.
- `just version-check` (H17), run as part of `just ci`: the release version must
  agree across `Cargo.toml`, `flake.nix` and `extensions/package.json`, so a
  half-finished bump fails the build.
- `RejectCode` on the wire (F8). Every `ServerMessage::Error` now carries a
  machine-readable reason — `HelloRequired`, `ProtocolMismatch`,
  `InstanceTokenRequired`, `InstanceMismatch`, `ConnectionLimit`,
  `SessionLimit`, `UnknownSession`, `SessionBusy`, `InputRefused`,
  `SessionCreateFailed`, `PaneOperationFailed`, `Unspecified` — alongside the
  human-readable `message`. The code is the contract; the message is prose and
  may be reworded freely. `PROTOCOL_VERSION` is now **11**, and
  `docs/DAEMON.md` documents the codes.
- Typed client errors (F8). `pty::PtyError` and `storage::StateError` are real
  enums with hand-written `Display` and `std::error::Error` (including
  `source`) impls, converted to `io::Error` only at the one boundary that still
  reports in it. `io::Result` now appears only where an `io::Error` is the
  honest answer.
- `storage::StateStore`, with `FileStateStore` and `MemoryStateStore` (F11).
  `runtime::run` takes a store instead of calling a global save function, so the
  save rate limit, the urgent-save bypass and the forced exit save are testable
  without touching a filesystem or the process environment.
- An `AgentStatusSource` seam with a file-backed implementation and an in-memory
  double (F10), completing the pair started by `BranchProbe`. The per-chat path
  cache moved onto the file-backed source, where it belongs.

- Per-client **session namespaces** on the daemon (A3). The protocol hello now
  carries a 64-bit instance token, allocated on first use (from `/dev/urandom`
  where available) and stored in `state.json`, and every session is keyed on
  `(instance, session)`. A connection can only see, attach to, resize, write to
  or stop sessions in its own namespace; the same token after a restart still
  reclaims exactly the panes it left behind. The protocol version was bumped for
  this, and a hello without a token is refused.
- A **startup confirmation** before replaying persisted command terminals (C1).
  Shell terminals still restore automatically; a terminal with a stored command
  line is left stopped and listed in a prompt that shows each command verbatim,
  with `y`/Enter to run them and Esc/`n` to leave them stopped (which is
  reported on the status line rather than left to guess at).
- Daemon **limits** (A10): at most 64 concurrent connections and 256 live
  sessions, both refused with a `ServerMessage::Error` rather than a panic, and
  a 120 s idle deadline on established connections. The client sends a `Ping`
  every 20 s so a legitimately idle-but-attached client is never mistaken for a
  gone one.

- A `?`/`F1` keybinding overlay (E4). Every binding and every command now comes
  from one `app::BINDINGS` table: the command palette filters it by
  availability, and the overlay groups the whole thing by section — including
  the rows the palette cannot run, such as `Ctrl+j`/`Ctrl+k` selection, `Ctrl+p`
  itself, prompt editing and the mouse. Before this, `Ctrl+p` (the discovery
  mechanism) and `Ctrl+Esc` (quit) were documented only in the README, and a
  test asserted the app showed no bindings at all. `F1` always opens the
  overlay; a bare `?` opens it only when no chat or terminal is selected, so it
  never swallows a key a running PTY was meant to get. The overlay scrolls on a
  short terminal and closes on any other key.
- A confirmation step in front of destructive deletes (E3). `Ctrl+q` and the
  "Delete selected item" palette entry now open `Prompt::ConfirmDelete`, which
  names the chat (with its message count) or the terminal (with the command it
  runs), and says on its own line when the parent workspace is removed along
  with it — the cascade `remove_workspace_if_empty` used to perform silently.
  Enter/`y` confirm, Esc/`n`/Ctrl+c cancel, and any other key does nothing. The
  confirmation is skipped only for an item that is provably empty: an idle chat
  with no stored messages and no live transcript, or a stopped shell terminal
  with a blank screen — and never when the workspace cascade applies.
- Readline-style prompt editing (E7). All four prompts share a `PromptInput`
  holding the text plus a character-index cursor, and bind Left/Right,
  Home/End, `Ctrl+a`/`Ctrl+e`, `Ctrl+w`, `Ctrl+u`, `Ctrl+k` and Delete through a
  single handler. Fixing a typo in the middle of the pre-filled working
  directory no longer means backspacing the whole tail. `Ctrl+k` keeps its
  "select previous match" meaning in the command palette and the
  configured-project prompt, and deletes to end of line in every other prompt;
  the choice is documented in the README. Filtering still reads the whole input,
  and the cursor is drawn by display width, so multi-byte and double-width
  characters put it in the right column.

- Command-line handling on both binaries, hand-rolled in the new `src/cli.rs`
  (no new dependency): `--help`/`-h`, `--version`/`-V`, and `--config`,
  `--state`, `--socket` path overrides. `mult --version` used to launch the TUI;
  an unknown flag now fails with a clear message and exit status 2 instead of
  starting a session. Flags take precedence over `MULT_CONFIG_PATH`,
  `MULT_STATE_PATH` and `MULT_SOCKET_PATH`, which take precedence over the
  default paths. `mult-server` rejects the client-only flags rather than
  accepting a flag it would ignore. `default-run = "mult"` makes a bare
  `cargo run` work again, as the README and justfile already assumed.
- A global status line. Runtime problems that belong to no pane — a daemon that
  could not be reached or reported a connection-wide failure, a save or draw
  that failed, the startup config warnings, the state-backup notice — are shown
  one at a time on a single row above the prompt, dismissible with `Ctrl+g` or
  the new "Dismiss status message" palette entry. The row only exists while
  there is something to say, so it never permanently costs output space, and
  levels are marked by shape (`x`/`!`/`·`) as well as colour. Previously the
  daemon-connection failure was queued against `PtyKey::Terminal(TerminalId(0))`
  — terminal ids start at 1 — so it rendered into a pane that cannot exist and
  a user with a missing or protocol-incompatible `mult-server` saw an inert UI
  with no explanation at all.
- A "Reload config" command-palette entry that re-reads the config file the
  session started from and swaps it in. A reload that fails reports through the
  status line and keeps the running config, rather than ending the session;
  `mouse_capture` is noted as taking effect only on the next start.
- `NO_COLOR` support: any non-empty value renders the whole UI in the
  terminal's default colours, with the sidebar selection and selected prompt
  rows switching to reverse video so they stay visible. No light-theme default
  was added; a light terminal still wants either `NO_COLOR` or explicit
  `colorscheme` keys.
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

- `nix flake check` passes. Beyond G15's six tests it needed two more things:
  the `ensure_private_dir` root fix above, and an opt-out for the nine
  `mult-server` dispatch tests that create a real pane — a Nix sandbox has a
  `/dev/ptmx` symlink with no `devpts` behind it, so `openpty` fails with
  `ENOENT` before any of `mult`'s code runs. They now honour the same explicit
  `MULT_SKIP_PTY_INTEGRATION` opt-out the integration suite already used, which
  `flake.nix` sets and nothing else does; everywhere a PTY can be allocated they
  run and must pass.
- The `vt_response_detector` fuzz target no longer floors its dimensions at 2.
  It floored them to stay off A13's upstream panic; now that `mult` clamps, the
  target hands it 0 and 1 as well and exercises the clamp.
- Documentation corrections found by re-checking every doc against the code:
  `MULT_STATUS_EXTENSION_SOURCE` and `MULT_CLAUDE_STATUS_SCRIPT_SOURCE` were
  documented as environment variables in the README and CONTRIBUTING, but are
  `include_str!` constants with no runtime override; the README said only Linux
  verifies socket peer credentials (every supported platform does, and an
  unsupported one refuses the connection); `docs/CONFIG.md` had the
  `pine`/`foam`/`iris` descriptions rotated by one against what those keys
  actually colour; `SECURITY.md` claimed the per-chat agent status files are
  owner- and mode-checked (they are not — the guarantee is on the directory) and
  still described the pre-11a `status: "running"` state field; three
  `TROUBLESHOOTING.md` headings omitted the prefix the message is actually shown
  with; and `docs/RELEASING.md` miscounted the hand-maintained version pins and
  omitted the pre-release tag pattern.

- The declared MSRV is now tested (H1). `Cargo.toml` said `rust-version =
  "1.88"` while CI and `rust-toolchain.toml` both pinned 1.94, so the promise
  was never exercised. A CI job now runs `cargo +1.88 check --workspace --locked
  --all-targets --all-features`; 1.88 was verified to still build the whole
  workspace, so the declared version stands rather than being raised.
- The release version is declared once (H17). `[workspace.package] version` in
  the root `Cargo.toml` is the source of truth and `crates/protocol` inherits
  it, removing one of the four hand-maintained copies; `just version-check`
  guards the two that remain.
- `cargo audit` was removed in favour of `cargo deny check` (H10). `deny.toml`
  already noted that deny supersedes audit, and running both meant a second tool
  re-checking a subset of the same RustSec data. `just ci`, the CI jobs, the Nix
  dev shell and the docs all follow; `just audit` is gone.
- Supply-chain checks now also run on a **weekly schedule** (H10), so an
  advisory filed against a dependency already in `Cargo.lock` surfaces without
  waiting for the next PR. `workflow_dispatch` was added alongside it.
- `.gitignore` covers `.claude/settings.local.json`, `*.corrupt-*` and
  `*.pre-11a-*` state backups, coverage and fuzz output, and common editor
  directories (H15). The first was previously ignored only by the repository
  owner's *global* gitignore, so every other contributor saw it untracked.
- `CONTRIBUTING.md` documents `MULT_SKIP_PTY_INTEGRATION` and `MULT_TEST_SHELL`,
  which gate the PTY integration suite and are set by `flake.nix` but were
  documented nowhere (H14). The README's environment table was audited against
  the code and now separates variables you set from the ones `mult` sets for the
  agent it launches; the deleted `MULT_AGENT_CMD` is gone from both.
- **Architecture, structural pass (Slice 11b).** Behaviour-preserving
  throughout: the `insta` snapshots of whole rendered frames are byte-identical
  and the test count is unchanged.
  - `runtime` moved into the library (`pub mod runtime`) and split into
    `runtime/{mod,input,prompts,keymap,mouse,clipboard,session,agent_launch,agent_status}.rs`
    (F9). It used to be `mod runtime;` in `main.rs` alone, so neither `tests/`
    nor the library could reach it and its test module carried a hand-copied
    duplicate of the library's own project fixture; that duplicate is gone.
  - New `src/layout.rs` owns `AppLayout::compute(&App, Rect)` (F6). The frame's
    geometry is resolved once per loop iteration — immediately before the draw,
    from the live terminal size — and the same value is handed to `ui::draw`,
    to the resize handlers and to mouse hit-testing. The renderer is no longer
    also the geometry oracle, and `layout_areas` is no longer recomputed four
    times a tick. A regression test pins that a visible PTY is resized exactly
    once per layout change, and to the rect the renderer draws into.
  - `ui` split into
    `ui/{mod,theme,vt_screen,sidebar,main_pane,selection,prompt,status,help,text}.rs`
    (F15), separating the WCAG contrast maths and the `vt100 → tui_term`
    adapter from the widget drawing. The snapshot files moved with the code to
    `src/ui/snapshots/` — `insta` resolves the snapshot directory from the test
    file's own directory, so `src/ui.rs` → `src/ui/mod.rs` moves it; the file
    *names* come from the module path (`mult__ui__tests__*`) and are unchanged.
  - `App`'s five overlapping optional fields collapsed into one
    `InteractionMode` (F5): a prompt and a pane focus can no longer both be
    live, and *which* pane is focused is derived from the selection instead of
    being stored, so `focus == Chat` with a terminal selected is
    unrepresentable. `App::prompt` and `App::focus` are now the `prompt()` and
    `active_focus()` accessors; `active_focus()` answers `None` while a prompt
    is open, replacing the `is_prompt_active()` check that five call sites each
    made for themselves. `app.rs` split into
    `app/{mod,nav,delete,prompt,open_workspace,text_input,search,selection,status,bindings}.rs`.
  - The `README.md` project-layout table and `extensions/package.json` now
    describe the real tree (H13).

- **Removed the unreachable process-agent transcript path** (F1), after proving
  it had no production caller: `AgentBackend::send_prompt` was only ever called
  from its own forwarding impl, there is no `dyn AgentBackend` anywhere, and so
  no `AgentEvent` could ever be produced, no `apply_agent_event` could ever run
  and `ChatSession::messages` could never be written. Chats have been driven by
  the PTY path since agent chats were introduced. `src/agent.rs`,
  `ChatBuffer`, `ChatMessage`/`ChatMessageRole`, `ProjectState::messages`,
  `append_chat_delta` and the "Saved transcript" pane branch are gone, and the
  `MULT_AGENT_CMD` environment variable — which was parsed and then never used
  to spawn anything — is removed from the docs.
- Chat search now searches the chat's agent PTY screen, the same lines the chat
  pane renders, through the same `App::search_matches` a terminal search uses.
  It previously filtered the persisted transcript, which nothing ever wrote, so
  it always matched nothing.
- Persisted terminal state records **intent, not liveness** (F16).
  `TerminalSession.status` is now `restore_on_launch: bool`, the two
  `mark_terminal_running`/`mark_terminal_stopped` sites collapse into one
  `set_terminal_restore_on_launch`, and how a terminal is *drawn* comes solely
  from `PtyRuntime`. Missing a call can now only cost a terminal its restore; it
  can no longer leave a dead terminal rendered as running. The startup
  confirmation for `Command` terminals is unchanged.
- The chat "seen this finished" bit moved into the status itself
  (`ChatStatus::Done { seen }`), replacing the `seen_done` side table and its
  reconciliation (F16). It is deliberately not persisted, so a finished chat is
  an unseen notification again after a restart — exactly what the old
  runtime-only set produced by starting empty. A status re-reported by the
  250 ms poller no longer re-arms a notification the user has already seen.
- `PtyRuntime::scroll_up`/`scroll_down` return `bool` instead of
  `io::Result<bool>`; they never left the process and could not fail.

- **Architecture, mechanical pass** (Slice 10): behaviour-preserving throughout,
  except for F18 below.
  - `PtyRuntime` keeps one `PtyPane` per PTY instead of eight parallel maps
    keyed by the same `PtyKey` (F2). Removing a PTY is now a single `remove`,
    where it used to be seven removals that had to be kept in step by hand — an
    omission left a deleted terminal's scrollback, exit status or command
    history behind with nothing to reclaim it.
  - Connecting takes an explicit `SpawnPolicy::{Autospawn, ConnectOnly}` instead
    of a `bool` threaded through three call layers (F3), so forking a daemon is
    never something a call site can do by accident.
  - `ChatId` and `TerminalId` have private fields and constructors that refuse
    the runtime half of the session id space (F4). The wire encoding moved into
    `mult_protocol::SessionId::{for_kind, split}`, and the inverse is fallible:
    a malformed pane id from the daemon is an error rather than a `ChatId` read
    as a `TerminalId`.
  - `ProjectState::default()` is empty and reads no environment (F12). The
    starter workspace is `first_run_seed(cwd)`, reached only when there was no
    state file at all — and it is now a single workspace named after the launch
    directory with one shell terminal, rather than the two demo workspaces
    (`mult` and `website`) a first run used to be given.
  - `ListSelection` holds each prompt's wrap-around result cursor and one
    `handle_common_prompt_key` serves all four prompts (F13); the sidebar's rows
    and its highlight come from a single walk in `App` (F14).
  - `PaneId` is gone: a session owns exactly one pane and the two ids were
    always equal, so `SessionId` names both and the linear scan that looked for
    a mismatch is deleted (A11).
  - `PtyKey`-typed API is spelled `pty` (`remove_pty`, `pty_lines`,
    `pty_output_is_blank`, `PtyEvent::Exited { pty, .. }`, …), reserving
    `terminal`/`TerminalId` for durable shell sessions, and the three
    identical alias pairs are deleted (F19).
  - `random_u64`, `invalid_data`, `default_shell`, `shell_command_args` and the
    two shell-quoting helpers live in `mult_protocol` once (F20).
- `PROTOCOL_VERSION` is **10**. `ClientMessage::{Paste, Scroll, ScrollToTop,
  ScrollToBottom}` are removed — no client ever sent them, pasting travels as
  `Input`, and scrolling is client-side against the local emulator — and
  `SessionInfo` no longer carries the `pane` field that duplicated its `id`.
  Stop any running `mult-server` after upgrading.
- Sidebar status is signalled by shape, not only by hue (E8). Chats render `*`
  thinking, `?` waiting, `!` failed, `✓` finished-and-unseen and `·` idle;
  terminals render `>` for a running command, `✓` for a clean exit, `!` for a
  non-zero or signalled exit and `$` when idle. Every chat used to be the same
  `●` and every terminal the same `$`, so a red/green colourblind user could not
  tell a clean exit from a crash. Colour still reinforces each state, and the
  glyphs are single-width characters with no Nerd Font or emoji dependency —
  which is also what makes the `NO_COLOR` mode genuinely usable rather than a
  screen of identical dots.

- Config errors name the file and the position. A bad config used to end the
  process with a `Debug`-printed `io::Error` and no filename
  (`Error: Custom { kind: InvalidData, error: Error("trailing characters",
  line: 9, column: 3) }`); it now prints `config error at <path>:<line>:<col>:
  <message>` on stderr and exits with status 2. `fn main` no longer returns an
  `io::Error`, so nothing reaches the user through `Debug` again.
- Config validation. `deny_unknown_fields` on the config and colorscheme
  objects means a typo like `auto_start_terminal` (missing the `s`) is reported
  instead of silently doing nothing, and the per-key colour parse failures that
  were already collected are now shown as startup warnings in the status line.
  Configured `projects[].path` entries are checked lazily where the
  open-workspace prompt lists them and marked `(missing)` rather than failing
  the load. The policy — undecodable file is a hard error, bad value warns and
  continues — is written down in the README.
- `state.json` decoding is lenient field by field: a missing or `null` key takes
  that field's default and an unrecognised key is ignored, so a renamed field or
  a `null` where a list was expected no longer discards every workspace, chat
  and terminal. When a reset really is unavoidable, the status line names the
  timestamped backup instead of silently presenting an empty project.
- Idle cost of the event loop is down by roughly an order of magnitude:
  - A `Resize` is only sent when the pane dimensions actually changed. The
    resize ran unconditionally once per tick per selected pane — a serialized
    message plus a socket write and flush, then a pane lock, a master lock and a
    `TIOCSWINSZ` on the daemon — 62.5 times a second with the window untouched.
  - Agent status polling resolves its runtime directory once per process and
    caches each chat's path, instead of re-running `ensure_private_dir` (a
    `mkdir` plus an `lstat` per ancestor) per chat per frame, and reads the files
    on a 250 ms timer rather than every tick.
  - After ~0.5 s with nothing to do, the loop's poll interval backs off from
    16 ms to 100 ms and snaps back on the first input, PTY event, agent event or
    status change. Keyboard latency is unaffected — `event::poll` returns as
    soon as an event arrives — so only the first chunk of a burst of PTY output
    after a fully idle period can be noticed up to one interval late.
  - State saves are rate-limited to ~1 Hz. Each save re-serializes the whole
    project and `fsync`s twice, and streaming agent output marks the state dirty
    on nearly every frame. Structural changes (a workspace, chat or terminal
    added or removed) and quit still save immediately.
- The git branch probe runs on a worker thread behind the new
  `git::BranchProbe` / `git::BranchWatcher`. It used to fork `git` once per
  workspace every two seconds on the render thread, costing 2-10 ms each — a
  10-50 ms input-latency hiccup with five workspaces, worse on a network
  filesystem. A refresh requested while one is in flight is skipped.
- Chat text is bounded: a persisted message stops growing at 128 KiB (marked
  `[mult: message truncated]`, never splitting a multi-byte character) and the
  in-progress display line flushes into the existing 500-line ring at 8 KiB. An
  agent streaming a build log used to grow both without limit, and every byte
  was rewritten and fsynced on every save.

- `PROTOCOL_VERSION` is 8: `ServerMessage::Error` now carries an optional
  `pane`. The daemon sets it wherever the failing pane is known (input, paste,
  resize, attach, stop, takeover eviction, and a create that named its session
  id), so the client can attribute a failure instead of guessing. Restart both
  binaries after upgrading.
- Per-pane scrollback retention dropped from 32 MiB to 5 MiB. The client renders
  at most 5 000 lines of scrollback, so the old cap kept far more than any client
  could show while costing 320 MiB resident across ten panes.
- The daemon copies each PTY chunk once fewer on its way to the client: the
  history append reads the read buffer directly, the payload is only built when a
  client is attached, and the last (normally only) attached client takes it by
  move instead of by clone. The per-read client snapshot reuses one buffer.
- CI installs `just` / `cargo-audit` / `cargo-deny` from prebuilt binaries
  instead of compiling them from source.
- `just ci` now matches GitHub Actions: new `just deny` (`cargo deny check`) and
  `just typecheck` (bundled status extension) recipes are folded into the gate.
  `typecheck` skips with a notice when `npm` or `extensions/node_modules` is
  unavailable, so the gate still runs offline.
- `just check` / `just test` / `just lint` pass `--locked`, so a `Cargo.toml`
  change that forgets `Cargo.lock` fails the gate instead of silently rewriting
  the lockfile.
- The Nix dev shell provides `cargo-deny`, which `CONTRIBUTING.md` already
  claimed it did.
- Path resolution for the socket, config file, and state file is split into pure
  functions that take the environment as arguments, so their precedence rules are
  tested without `set_var`/`remove_var` on a process global — a real race against
  sibling tests reading the same paths on other threads.
- The client's terminal-response detector keeps the CSI sequence in an inline,
  already-bounded buffer instead of heap-allocating a `Vec` on entering every CSI
  — thousands of allocations per frame while a full-screen TUI child redraws.
- Escape sequences are fed to vt100 in whole spans rather than one byte per
  `process` call. Only the printable run was batched before, so escape-dense
  output (the common case) paid vte's full dispatch setup per byte. A new test
  pins the batched feed to a byte-at-a-time reference feed — screen, cursor and
  replies — over an adversarial corpus plus a seeded generator.
- `PtyEvent::Output`/`Scrollback` carry a byte count instead of the payload; the
  bytes were cloned into the event queue for every chunk and no consumer read
  them. The client-side event queue also shrank from 4 096 to 256 entries (a
  32 MiB worst case if the UI thread stalled), and adjacent output chunks for the
  same pane are coalesced into one parser feed on drain.
- New `crates/protocol/tests/framing.rs` covers the wire codec: seeded
  round-trips of both message enums, payloads split one byte per `read` (the real
  socket case), truncated frames, empty frames, oversized frames and malformed
  postcard bytes.
- `src/storage.rs` tests for the recovery paths that decide whether a user keeps
  their workspaces: a serde *shape* error (valid JSON, wrong types) is backed up
  byte-for-byte before the reset, an older `version` is upgraded in memory
  without touching the file, and a backup rename that cannot happen (read-only
  parent) surfaces as an error with the state file left intact.
- `src/agent.rs` tests beyond the happy path: spawn failure reports an error
  event and leaves the target startable, a nonzero exit reports an error rather
  than `Done`, a full event queue cannot block a pipe reader, and pipe output
  survives both invalid UTF-8 and multi-byte characters split across reads.
- Rendering a frame costs roughly half of what it did, with two thirds fewer
  allocations. Measured on a full 50×200 pane, per frame:
  - The vt100 screen copy went from 673 µs / 19 807 allocations to 361 µs /
    9 801. Each cell's symbol was allocated twice — `vt100::Cell::contents()`
    already returns an owned `String` and the result was `to_string()`d again —
    and the copy then held ten thousand live `String`s until the next frame
    dropped them. The symbol is now stored inline (24 bytes, the widest a vt100
    cell can hold, with a heap fallback that vt100 cannot reach), and blank
    cells skip `contents()` entirely instead of allocating an empty string.
  - The whole frame went from 1.105 ms / 29 985 allocations to 695 µs / 9 835.
    The remainder is the search-line scrape: `terminal_all_lines` (125 µs /
    10 144 allocations) was passed as an eager argument to
    `App::terminal_search_matches`, which returns on its first line when no
    search is active — so the entire screen was scraped every frame for a
    filter that was almost never running. It is now behind a closure.
  - `truncate_text` measures characters with `char::encode_utf8` into a stack
    buffer instead of `char::to_string()`, removing an allocation per character
    on two sidebar strings per row per frame.
- The colorscheme is parsed once per config instead of on every frame:
  `Config::colors()` memoizes the twelve `#rrggbb` values, and the renderer's
  palette is a field-for-field copy of the result. The memo starts empty and
  fills on first use, so a config assembled from another one cannot inherit a
  stale palette.
- The Rosé Pine Moon defaults exist once, as `config::DEFAULT_COLOR_SCHEME`. The
  palette used to be written twice — hex strings in `src/config.rs` and
  `Color::Rgb` constants in `src/ui.rs` — with nothing enforcing that they
  agreed; the strings are now spelled from the table and the renderer's
  fallbacks come from the same place. A colorscheme key that does not parse
  still falls back to its default, but the failure is now returned by
  `ColorSchemeConfig::resolve` / `Config::colorscheme_errors` instead of being
  discarded, ready for startup validation to report.
- Full-buffer snapshot tests (`insta`, a dev-dependency) over the default frame,
  the command palette, a scrolled terminal with a text selection, and an 80×24
  layout. Every previous UI test grepped a single row for a substring, so a
  sidebar that changed width or a prompt that grew a line passed all of them.
  Each snapshot pins both the symbol grid and a background-colour grid, and
  renders a fixed fixture with a stubbed git branch so nothing in it varies with
  the machine. Direct tests for the vt100 adapter came with them: attribute →
  `Modifier`, `vt100::Color::{Default,Idx,Rgb}` → ratatui, wide cells, combining
  marks and inline-vs-heap symbol storage.

### Fixed

- **A PTY pane one row or one column in size no longer panics the client**
  (A13). `fnug-vt100` subtracts past zero on a one-row grid the moment a line
  wraps (`grid.rs:637`) and on a one-column grid the moment a double-width
  character is measured (`screen.rs:788`), so a stray non-UTF-8 byte, an emoji
  or an ordinary long line took the TUI down with the terminal left in raw mode
  — reachable simply by making the window small enough. This is an upstream
  defect; `mult` works around it by making the size unrepresentable. The floor
  (2×2, established by probing, not guessed) now lives in
  `mult_protocol::bounded_screen_dimensions` and `PtyDimensions`, whose fields
  are private, so it applies to every screen the client builds or resizes, to
  every size it puts on the wire, and to the fuzzing seam. The daemon clamps a
  `Resize` the same way, since a client can ask for one row and the two ends
  must agree on the size the child draws for.
- **A pane too small to show a screen now says so** rather than showing the
  top-left corner of one. Below 2×2 the pane renders `too small` (or nothing,
  where even that will not fit); the PTY keeps running at the floor.
- `ListSelection::step` is a true modular wrap in both directions for any step
  size (F21). The backwards branch was
  `index.checked_sub(delta).unwrap_or(len - delta)`, which agrees with a wrap
  only for a step of one — at `len = 5, index = 1, delta = -3` it landed on `2`
  where a wrap gives `3`. No caller stepped by more than one, so nothing was
  visibly wrong; the next one to be added would have been.
- Six `runtime::agent_launch` / `runtime::agent_status` unit tests no longer
  depend on ambient directory privacy (G15). The agent command builders and the
  file-backed status source take their directory as an argument, so the tests
  supply one they created themselves; none of them sets an environment variable.
- `mult_protocol::ensure_private_dir` no longer judges the filesystem root
  (G15). Inside a user namespace `/` can be owned by neither the caller nor
  root — a Nix build sandbox maps it to `nobody` — and vetting it rejected
  *every* directory on the system, including one just created 0700 by the
  caller. An attacker who owns `/` owns every alternative the check could offer,
  so nothing is given up; every component below the root is checked exactly as
  before.
- `README.md` and `CONTRIBUTING.md` no longer link to `docs/REMAINING_WORK.md`,
  which does not exist; both point at `docs/BACKLOG.md` (H6).
- `CHANGELOG.md` renders (H12). `[Unreleased]` and `[0.1.0]` used
  reference-link syntax with no link definitions, so both showed as literal
  square brackets; the definitions are now at the bottom and `[0.1.0]` carries
  its date.
- The client no longer decides *anything* by matching on error text. It used to
  pick an `ErrorKind` with `message.contains("already attached")` against a
  string the daemon happened to `format!` — and that had already broken in
  silence: session takeover replaced the rejection that produced those words, so
  the branch stopped ever being taken and nothing failed (F8).

- A chat backed by Claude Code is no longer told that "Pi agent" has not started
  and pointed at `pi_agent_command`/`auto_start_pi_agent`, which have no effect
  on it (F18). The hint names the chat's own agent and its own config keys.
- Recovering from a corrupt `state.json` presents an *empty* project (F12). It
  used to hand back the demo seed, so a user whose state was damaged was given a
  fabricated `website` workspace containing a terminal they never created —
  alongside a notice saying the session was "starting empty".
- Dead surface removed (F17): `terminal_all_lines`, `ensure_parser`,
  `scroll_to_top`/`scroll_to_bottom`, the three `*_search_status` helpers, the
  test-only focus cycle, `ProcessAgentBackend::command`, `AgentEvent::target`,
  `AgentMessageRole::label`, and the two no-op pane-inset helpers whose header
  constants were both zero. `ListSessions`/`SessionInfo`/`PaneInfo` were kept:
  the daemon implements them and its tests observe eviction, namespace isolation
  and the session cap through them.
- The client no longer hangs forever when a pane stops reading its input (A2).
  The daemon handled `Input`/`Paste` with a blocking `write_all` on the PTY
  master *on the connection's reader thread*: a child that stops reading stdin
  fills the master's few-KiB input buffer permanently, so the daemon stopped
  reading the socket, the socket buffer filled, and the client's render thread
  blocked in its own `write_all` — both ends wedged, reproducibly, by pasting a
  large buffer into such a pane. Each pane now owns a writer thread behind a
  bounded 64-chunk queue that the socket reader only ever `try_send`s to. Input
  for a pane whose queue is full is refused with an error naming that pane
  (never dropped in silence), and the client additionally bounds any socket
  write at 5 s and drops the connection rather than blocking the UI on it.
- Connecting to the daemon is off the render thread (B6). Connect, autospawn
  wait and protocol hello all happen on a worker thread; the render loop polls
  the result once per frame, reports failures through the status line and says
  "reconnected to mult-server" when a reported outage ends. A keystroke could
  previously cost up to ~6 s (2 s socket wait + 2 s hello + 2 s attach
  acknowledgement), and the frame on which a connection dropped paid another
  connect+hello. A terminal started while the connection is still coming up is
  queued and launched the moment it lands — or retired with an exit event if it
  never does — instead of failing outright.

- A finished chat no longer springs back to "thinking" forever. The per-chat
  agent status file was only removed when a chat was (re)started, so after the
  agent exited and the PTY exit set `Done`/`Failed`, the very next poll re-read
  the leftover file — last written as `running` — and flipped the chat back.
  The file is now deleted when the agent PTY exits, and a chat whose PTY is not
  running ignores its status file entirely.
- The agent event channel can no longer deadlock the UI. `send_prompt` sent
  status events with a blocking `SyncSender::send` from the render thread, which
  is the only thread that drains that queue; with the pipe readers holding the
  1 024 slots full, the UI blocked forever. Both the backend and the pipe
  readers now use `try_send`: backend events that do not fit are held and
  emitted by the next drain, and dropped pipe output is counted and reported in
  the transcript (`[mult: dropped N bytes of agent output]`).
- Agent output is decoded across read boundaries. An 8 KiB read that split a
  multi-byte character turned it into U+FFFD, which was then persisted; the
  incomplete trailing sequence is now carried into the next read. Genuinely
  invalid bytes are still replaced, so a binary pipe cannot wedge the reader.
- Killed agent processes are reaped. `Drop` sent a kill without waiting, leaving
  a zombie per agent for the lifetime of the process.
- A wrapped or hand-edited id counter can no longer hand out an id that a live
  chat or terminal already holds: allocation consults the used ids, exactly as
  the state-file repair path already did.
- A save or draw failure no longer kills the session. A transient `io::Error`
  from `save_if_dirty` (full disk, read-only `$XDG_DATA_HOME`, a temp-file
  collision) propagated out of `run` and took the in-memory state with it.
  Failures are recorded on `App` (surfaced by the status line a later slice
  adds), the state stays dirty so the next save retries, and the exit save is
  forced past the rate limit — including when the loop ends on a draw error, so
  nothing edited since the last save is lost. Only a repeatedly failing draw
  (three in a row) or a failing exit save still ends the session.

- The client no longer materialises a terminal for a pane id it never attached.
  Any pane id the daemon mentioned used to be turned into a `PtyKey`, so late
  output for a terminal that `Stop`, a delete or a `PaneExited` had just dropped
  resurrected a 5 000-line scrollback parser that nothing ever reclaimed,
  deleted terminal content could reappear, and a rogue or stale daemon could
  exhaust client memory by streaming output for distinct pane ids. Only `Attach`
  establishes a mapping now; output for an unmapped pane is dropped.
- Reconnecting shuts the previous socket down (`shutdown(Both)`) before adopting
  the new connection. The old reader thread used to stay parked in
  `read_message` on a still-open fd until the server independently evicted it,
  so every reconnect leaked a thread and a pair of file descriptors.
- The per-frame event drain has a work budget (128 messages / 256 KiB). It used
  to drain until the queue was empty, so a pane producing output faster than
  vt100 consumes it refilled the queue as fast as it drained and the frame never
  completed — the UI stopped responding to input. Leftover work stays queued and
  is reported to the loop so the frame is still drawn.
- A daemon error that names no pane is no longer written into an arbitrary
  terminal. It used to be attributed via `pane_to_terminal.values().next()` — a
  nondeterministic `HashMap` pick — or to terminal id 0, which never exists. An
  error naming a *different* pane also no longer aborts an in-flight attach.
- Terminal query auto-responses (`CSI c`, `CSI 5n`, `CSI 6n`, `ESC Z`) are capped
  per input chunk (at most one cursor report, eight replies in total) and the
  replies for a chunk go out as a single `Input` message. A pane printing
  `\x1b[6n` in a loop produced roughly 2 700 protocol messages per 8 KiB chunk,
  each its own socket write on the render thread.
- `scroll_up` clamps its row count before narrowing to `i32` (as `scroll_down`
  already did); a count with bit 31 set used to invert the scroll direction.
- `read_message` grows its payload buffer as bytes arrive instead of committing
  the declared frame size up front, so a peer that writes only a 4-byte length
  header cannot park a reader on a 16 MiB allocation, once per connection. A
  zero-length frame is now rejected explicitly.
- `mult-server` no longer memmoves the whole retained scrollback on every PTY
  read. The per-pane history was a flat `Vec<u8>` trimmed with
  `drain(..overflow)`, so once it reached its cap each 8 KiB read copied ~32 MiB
  (measured 2.27 ms per read, a ~3.6 MB/s per-pane ceiling) — while holding the
  pane mutex, stalling attach, input and resize for every client. It is now a
  deque of chunks whose trim costs time proportional to the bytes dropped, not
  the bytes kept.
- A single failing pane operation no longer tears down the whole client
  connection. An `EIO` write to a pane whose child had exited, a duplicate
  session id, or a `Stop` for a pane the client no longer knows about used to
  propagate out of the message loop, disconnecting *every* pane on that
  connection and forcing a full parser reset. Such failures are now reported as
  an error for that pane and the connection keeps serving.
- Stopping a pane kills the pane's process groups (SIGHUP, then SIGKILL after a
  grace period) instead of sending SIGKILL to a single pid. Grandchildren left
  running on the terminal used to survive holding the PTY slave open, so the
  pane's reader thread never reached EOF and leaked the thread, the master fd and
  the pane's entire history for the daemon's lifetime.
- A client evicted from a pane by another client's takeover is now told about it
  (an error plus `PaneExited` for that pane) instead of being dropped from the
  pane in silence, which left it listing the session and rendering a terminal
  that would never update again.
- Attaching no longer copies the whole history under the pane lock, and the
  scrollback replay no longer disconnects the client it has just attached. A
  client with any pre-existing queue backlog used to overflow on the replay, get
  disconnected, reconnect, re-attach and overflow again. The replay now waits for
  the client's writer to drain and only gives up on the replay itself.
- The daemon polls the pane's foreground process (a pane lock, a master lock, a
  `tcgetpgrp` ioctl and a `/proc/<pid>/cmdline` read) through the existing
  debouncer instead of once per 8 KiB of PTY output — roughly 4 000 ioctls and
  file reads per second under load before.
- The PTY integration suite can no longer pass vacuously. Harness failure
  (missing `mult-server`, a failed spawn, a socket that never appears) used to
  return `None`, which every test turned into an early `return` and a green
  result, so the entire daemon reconnect/attach/exit safety net could be dead
  without anyone noticing. Setup failure now panics with a diagnostic, and the
  explicit `MULT_SKIP_PTY_INTEGRATION` opt-out — the only remaining skip path —
  prints a visible notice.
- De-flaked the wall-clock-dependent tests: deadlines raised from 2-5 s to
  15-30 s (each poll loop still returns as soon as its condition holds, so the
  happy path is unchanged), the `sleep 2` in the scrollback-replay test replaced
  with a long-lived command, and the SIGHUP test now proves the session survived
  by driving it with real input instead of racing `sleep 1`/`sleep 3` against the
  signal. The autospawned daemon's socket wait grew from 2 s to 15 s for the same
  reason.
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

### Known issues

- Resizing a pane **narrower** can crash the client, if the shrink cuts a
  double-width character (CJK, emoji) in half at what becomes the last column.
  `fnug-vt100` leaves the character's first half there with no continuation
  cell, and the next character printed at that position panics inside the
  parser (`screen.rs:928`, `called Option::unwrap() on a None value`) — taking
  the TUI down with the terminal in raw mode. This is not an edge-case size: 80
  columns down to 41, or to 79, reproduces it with CJK on screen. It is a
  different upstream defect from the one fixed below and the size clamp does not
  cover it; no correct workaround fits in this release. Filed as **A14**, with
  the three candidate fixes written down.

### Security

- `state.json` is treated as an execution boundary (C1). A terminal stored as
  `running` with a command line used to be handed to `$SHELL -lc` at startup
  with the command neither shown nor approved, which turned any write to the
  state file — a synced dotfile repository, a shared `$XDG_DATA_HOME`, any
  same-uid process — into code execution on the next start. Replaying one now
  requires a confirmation that prints the exact command. `SECURITY.md` and
  `AGENTS.md` say so, including the part that is still not confirmed (the
  workspace `cwd` and `environment` a restored terminal inherits).
- A same-uid process can no longer steal a live PTY stream just by speaking the
  protocol and guessing a session id (C12). Sessions are namespaced by the
  instance token from the client's state file, and `Attach` cannot name a
  session outside the connection's own namespace. The trust boundary is still
  the uid — an attacker who can read `state.json` can read the token — but
  attach is no longer takeover-by-default for *anyone* who connects.
- The daemon caps connections and sessions and drops connections that go silent
  (A10), so a same-uid loop of `CreateSession` or of idle connections can no
  longer exhaust memory, PIDs, file descriptors and threads and take out every
  pane the user has.
- Peer verification on the client/daemon socket now works on **every** Unix, not
  just Linux, and can no longer be silently skipped. `peer_uid()` used to return
  "unknown" on macOS and the BSDs and both callers read that as *accept*, so a
  squatted socket at an inherited `$MULT_SOCKET_PATH` saw every keystroke in
  every pane. The check is `SO_PEERCRED` on Linux and `getpeereid(3)` on
  macOS/BSD, a platform that offers neither refuses the connection, and both
  binaries share one implementation in the new `mult-protocol::peer` module
  (the check was previously copy-pasted into each, each with its own tests).
- `config.json` and `state.json` are read with `O_NOFOLLOW`, must be regular
  files owned by the current user with no group/other write bit, and are size
  capped. Both paths are environment-overridable and both are execution
  channels — the config's `pi_agent_command`/`claude_code_command` are shell
  evaluated and auto-started by default — so a planted symlink or a foreign
  write at either path used to be silent code execution, and a FIFO or a
  `/dev/zero` link could hang or OOM startup.
- The per-chat agent status directory fails closed. It used to fall back to the
  exact directory the privacy check had just rejected, then export it to the
  spawned agent as `$MULT_AGENT_STATUS_PATH` and `remove_file` inside it. Status
  reporting is now skipped entirely in that case and the reason is surfaced.
- An autospawned `mult-server` must be a regular file owned by the user or root
  with no group/other write bit, and is started with a minimal environment
  (`PATH`, `HOME`, `SHELL`, `USER`, `LOGNAME`, `TERM`, `LANG`, `LC_*`, `MULT_*`)
  instead of the client's. The daemon outlives its client and seeds every later
  PTY, so it used to hand the first session's API keys to every terminal of
  every later client and workspace.
- The per-workspace branch probe no longer runs `git`. It reads `.git/HEAD`
  directly (bounded, regular-files-only, following `gitdir:` pointers), so
  merely opening a hostile repository can no longer cause its `.git/config` to
  be parsed — `include.path`, `core.fsmonitor` and `core.hooksPath` are all
  code-execution vectors, and the probe ran automatically every 2 s.
- The status writers in `extensions/mult-claude-status.sh` and
  `extensions/mult-status.ts` no longer write through a predictable
  `<path>.<pid>.tmp` created without `O_EXCL`/`O_NOFOLLOW`, where a pre-planted
  symlink redirected — and truncated — the target. The shell hook uses `mktemp`;
  the extension uses `openSync` with `O_CREAT|O_EXCL|O_WRONLY|O_NOFOLLOW` and a
  random suffix, creates its directory `0700` (not the default `0755`), writes
  `0600`, and refuses a parent directory that is not owner-only.
- Generated hook scripts, Claude Code settings and the pi status extension are
  named by content instead of by pid and a random suffix, so a given build
  writes one of each and reuses it rather than accumulating executable content
  in the runtime directory on every agent start. Per-chat status files are
  removed on shutdown.
- Server-supplied strings (`ServerMessage::Error` text, `ExitInfo::signal`) have
  their control characters stripped before reaching the pane emulator, so a
  rogue daemon can no longer clear a pane, move its cursor, or forge a `[mult]`
  status line. Branch names read from a repository are stripped the same way.
- OSC 52 clipboard writes can be turned off with the new `clipboard_osc52`
  config option (default `true`, i.e. unchanged behaviour) and are emitted
  through the terminal's own writer after a frame, instead of straight to
  `io::stdout()` from inside a mouse handler.
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

## [0.1.0] - 2026-05-19

Initial prototype: a Ratatui/Crossterm client plus a persistent `mult-server`
PTY daemon over a Unix socket — multiple workspaces with `pi` agent chats and
shell/command terminals, persistent JSON project state, terminal scrollback,
mouse selection, and OSC52 clipboard copy.

<!--
Link definitions. Without these, the `[Unreleased]` and `[0.1.0]` headings above
render as literal square brackets rather than links (H12).

`v0.1.0` is not tagged yet, so the two links below resolve only once it is
pushed; see docs/RELEASING.md. When cutting a release, move the `[Unreleased]`
compare link to the new tag and add a line for the version being released.
-->

[Unreleased]: https://github.com/Jofr3/mult/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/Jofr3/mult/releases/tag/v0.1.0
