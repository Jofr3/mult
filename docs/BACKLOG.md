# mult — improvement backlog (v2, rebased)

The v1 audit (`docs/BACKLOG-v1.md`) was run against commit `0224885`. `main` had meanwhile
advanced to `2a91632` with ~14.7k lines of independent, overlapping rework. Every v1 item was
re-triaged against the current code; this file is the result, plus 15 findings the v1 audit
never saw because the code did not exist yet.

**Do not read v1 as current.** It is retained only as evidence for items that carried over.
The v1 fixes live on branch `hardening/audit-remediation` (`f451667`) and can be consulted
with `git show hardening/audit-remediation:<path>` — but many no longer apply as written.

Severity: **C** critical · **H** high · **M** medium · **L** low.
Status: `todo` · `wip` · `done` · `dropped`.

## Triage outcome

| | count |
|---|---|
| v1 items **already fixed** by current `main` | 17 |
| v1 items **obsolete** (code restructured away) | 3 |
| v1 items **still applying** (some in changed form) | 82 |
| v1 items **deliberately abandoned** (v1's remedy is now wrong) | 1 |
| **new** findings from this triage | 15 |

## Decisions recorded

- **F1 — abandoned, do not port.** v1 proved the `agent.rs` / `ChatBuffer` / `ProjectState::messages`
  transcript path unreachable and deleted ~700 LOC. That analysis still holds — there is no
  `dyn AgentBackend`, so the call graph is closed and `send_prompt` has no production caller.
  But `main` now builds on those types: `src/transcript.rs` (a versioned, size-capped,
  truncation-recovering append-only journal) needs `ChatMessageRole`, and
  `docs/REMAINING_WORK.md` records it as a **completed Phase 3 deliverable** with Phase 3.4
  planning to feed it and §6.3 listing the dead-code question as an *open* decision.
  Merging v1 would have broken the build and destroyed finished work. The residual action is
  documentation honesty only — see E12.
- **C1 — `main`'s fix supersedes v1's.** `main` never relaunches a persisted
  `TerminalLaunch::Command` (`recoverable_terminals` + `terminal_requires_recovery`). v1
  prompted and then replayed, and that prompt was itself found broken by review. Keep `main`'s.
- **A3 / C12 — `main`'s `SessionIdentity` supersedes v1's instance token.** A 128-bit
  `StateNamespace` plus 128-bit per-session `SessionToken` already provides both the
  cross-instance namespacing (A3) and the per-session capability check (C12). Do not add a
  second mechanism.
- **`PROTOCOL_VERSION` — both reached `10`, meaning different wire shapes.** `main`'s 10
  (`u16`) is authoritative. Any wire change here renumbers from that, and all wire changes in
  this effort must land in a **single** bump, not one per slice.
- **F11 — same name, opposite purpose.** `main`'s `storage::StateStore` is a concrete
  process-lifetime *ownership lock*. v1's was a *testability* trait. Do not shadow or replace
  the lock. The real defect is narrower — see B16.

---

## A. Daemon (server side)

| ID | Sev | Where | Item | Status |
|----|-----|-------|------|--------|
| A2 | C | `mult-server.rs:1656-1658,2752-2758`; `pty.rs:1676-1689` | **Worse than v1 described.** `handle_leased_input` does a blocking `write_pty_input` (`write_all`+`flush`) to the PTY master **while holding both the global `ServerState` mutex and the pane mutex**. A child that stops reading stdin now freezes the *entire daemon* — every pane, every client — not just one connection. Client side still blocks on `write_message` from the render thread. Needs a per-pane writer thread with a bounded queue **and** the locks dropped before writing. | todo |
| N1 | C | `mult-server.rs:1479-1524,1551-1596` | Attach replay runs with the global server mutex **and** the pane mutex held, chunking up to 32 MiB into 512 × 64 KiB `to_vec`s plus channel sends. One client attaching to a busy pane serializes the whole daemon for the duration. Overflow still `disconnect()`s the client it just attached → reconnect loop. (Supersedes A7.) | todo |
| N2 | H | `mult-server.rs:235-244` | Daemon shutdown can hang forever: after `begin_daemon_shutdown` the main thread spins on `sessions.is_empty()` with a 20 ms sleep and **no deadline**. A pane whose stop driver reports `TimedOut` is never removed, so `mult-server` never exits and never unlinks its socket. | todo |
| A1 | H | `mult-server.rs:2674-2698` | `append_raw_history_with_limit` still `extend_from_slice` + `drain(..overflow)` on a flat 32 MiB `Vec<u8>` — O(history) memmove per 8 KiB read under the pane lock. Replay at `:1573` iterates `raw_history.chunks()`, so a chunk deque changes that path too. | todo |
| N3 | H | `mult-server.rs:1551-1596` | Per-attach memory amplification: 32 MiB of history becomes ~512 queued `ReplayChunk` `Vec`s resident in the client's 1024-slot channel *simultaneously*, on top of the pane's own 32 MiB — ~64 MiB per attaching client per pane. | todo |
| A8 | M | `mult-server.rs:2160` | `broadcast_foreground_process_if_changed` still called after **every** 8 KiB read: pane lock + master lock + `tcgetpgrp` ioctl + `/proc/<pid>/cmdline`. The debounced `schedule_foreground_process_poll` already exists and is used from the input path — the reader-thread call just needs deleting. Trivial now. | todo |
| A10 | M | `mult-server.rs:216-226,937,320-350` | No client cap, no session cap, read timeout cleared after `Hello`, no idle deadline, no keepalive. A same-uid `CreateSession` loop exhausts memory/PIDs/fds. Keepalive is a wire addition. | todo |
| A11 | M | `mult-server.rs:401-412,2566-2583` | `PaneId`/`SessionId` still distinct and always equal, and `pane_by_id`'s dead linear-scan fallback takes **every** pane mutex *while holding the server lock*, on every `Input`/`Resize`/`Detach`. With A2 that is a deadlock multiplier, not just waste. | todo |
| A9 | L | `mult-server.rs:2152,2688` | Down from 4 copies of every PTY byte to 3 (the per-client clone and client-vec clone went away with the lease model). `Arc<[u8]>` in the broadcast path is the remainder. | todo |
| A12 | L | `mult-server.rs:50` | `RAW_HISTORY_MAX_BYTES = MAX_MESSAGE_BYTES * 2` = 32 MiB resident per pane, unrelated to actual scrollback need. | todo |
| A13 | M | `pty.rs:397,1263`; `runtime.rs:1875`; `protocol/lib.rs:763` | A 1-row or 1-column pane panics `fnug-vt100` ("attempt to subtract with overflow") on a stray non-UTF-8 byte or an emoji. All sites still clamp `.max(1)`; `bounded_screen_dimensions` has no minimum. Found by v1's fuzz target. `PtyDimensions` fields are `pub` here, so v1's private-constructor fix needs rework. | todo |
| A14 | M | `pty.rs:1263` | Second `fnug-vt100` panic, never fixed on v1 either: narrowing a screen that holds a double-width char in the last column unwraps `None` on the next print. Reachable by dragging a window narrower with CJK/emoji on screen. Upstream defect; needs a workaround decision. | todo |

**Fixed by `main`, do not re-do:** A3 (`SessionIdentity` namespacing), A4 (`TakenOver` notification),
A5 (per-pane failures return `LeaseRejected`, no connection teardown), A6 (`drive_termination`
signals both process groups, SIGTERM→SIGKILL).

## B. Client PTY / runtime

| ID | Sev | Where | Item | Status |
|----|-----|-------|------|--------|
| N4 | H | `pty.rs:1499-1537` via `drain_events` → `reconnect_or_report` | On reconnect, **every** terminal is re-attached serially, each with its own 2 s `ATTACH_ACK_TIMEOUT`, on the render thread. A dead daemon with N terminals freezes the UI for ~2N seconds *every frame it retries*. | todo |
| B6 | H | `pty.rs:558,1471,1499,2348` | Blocking connect/hello/create/attach on the render thread: ~8 s worst case for one start. **Conflict:** `main` rebuilt the client as strictly synchronous request/response with idempotency keys and request caching, so only *connection establishment* can move off-thread — the correlated waits cannot, without a redesign. | todo |
| B9 | M | `app.rs:1181`; `runtime.rs:236,2385`; `storage.rs:479` | No cap on `ChatMessage.text`; `save_if_dirty_with` runs every tick when dirty, doing `to_string_pretty` of the whole project + `sync_all` + rename + directory `sync()` — once per streamed delta. No rate limit. | todo |
| B16 | M | `runtime.rs:224,236,269,1442` vs `storage.rs:33,339` | **New.** `main` acquires a lock-holding `storage::StateStore` in `main.rs`, but `runtime.rs` still saves through the *free* `storage::save`, which re-derives the path from env at call time. Two save paths, only one holding the lock — split-brain. (This, not v1's F11, is the real defect.) | todo |
| B3 | M | `runtime.rs:236,2122,2203` | The `ensure_private_dir` storm is gone, but the status journal is still polled every ~16 ms tick per chat with a live agent: `open(O_NOFOLLOW)`+`fstat`+`seek`+`read_to_end`+`close`. Needs a ≥250 ms timer. | todo |
| B5 | M | `pty.rs:665,901,1242,1632,1671` | Overwrite-without-shutdown is gone, but every `self.connection = None` drops the socket without `shutdown(Both)`, so the old reader thread stays parked on a live fd. Thread/fd accumulation across reconnects persists. `ServerConnection.writer` is a `UnixStream` clone, so the fix is available. | todo |
| B7 | M | `pty.rs:893-910` | `drain_events` loops `try_recv` until `Empty` with no byte or message budget. | todo |
| B8 | M | `pty.rs:1392-1398` | `ServerMessage::Error` still attributed via `pane_to_terminal.values().next()` or `Terminal(0)`. **Changed fix:** the protocol now documents `Error` as connection-wide and per-pane failures use `LeaseRejected`, so adding a `pane` field is wrong — route to a status surface instead (needs E2). | todo |
| B11 | M | `runtime.rs:244,250,252` | Save-failure half is fixed (`record_save_failure` + `cancel_quit`). Still fatal: `terminal.draw(...)?`, `event::poll(...)?`, `event::read()?` propagate out of `run` and kill the session, skipping cleanup. | todo |
| B4 | M | `agent.rs:144-146,177,181` | `send_event` still blocking `SyncSender::send` from the render thread. (Low practical impact while F1's path is unwired, but it is a live trap for Phase 3.4.) | todo |
| B10 | M | `agent.rs:303` | `from_utf8_lossy` on raw 8192-byte read boundaries; no carry of a trailing partial sequence. Same caveat as B4. | todo |
| B15 | L | `protocol/lib.rs:734` | `vec![0; len]` commits up to 16 MiB before any payload byte arrives. | todo |
| B12 | L | `pty.rs:840-842` | `scroll_up` does `rows as i32` unsaturated; `scroll_down` clamps correctly. | todo |
| B13 | L | `agent.rs:252-257` | `Drop` kills without `wait()`; zombies. | todo |

**Fixed by `main`:** B1 (`key_for_pane` returns `Option`, no parser synthesis), B2 (generation-stamped
status journal, daemon authoritative, no `Done`→`Thinking` resurrection). **Obsolete:** B14
(`next_durable_candidate` replaced by a `BTreeSet` search that cannot wrap).

## C. Security

| ID | Sev | Where | Item | Status |
|----|-----|-------|------|--------|
| C3 | H | `mult-server.rs:702-745`; `pty.rs:2213-2256` | Unchanged from v1: `peer_uid` returns `Ok(None)` off Linux and both callers treat it as **accept**. Squatted-socket keystroke capture on macOS/BSD still holds. Needs `getpeereid`, fail-closed, and one shared implementation (F7). | todo |
| C2 | H | `config.rs:126,140` | `load_from_path` is still a bare `fs::read` — symlink-following, unbounded, no owner/mode/regular-file check — and its `pi_agent_command`/`claude_code_command` go to `$SHELL -lc` and auto-start by default. `main` hardened *state* reads but not *config*. Use the existing `SecureDirectory` / `validate_private_regular_file`. | todo |
| S2 | M | `transcript.rs:190-193`; `storage.rs:640-642` | **New.** `TranscriptJournal::open` has two destructive side effects on a caller-supplied path: `read_and_recover` calls `file.set_len()` on any file whose tail lacks a newline, and `open_parent(.., normalize_parent: true)` **`fchmod`s the parent directory to 0700**. A mistyped path silently truncates a file and changes a directory's mode. Unwired today (S1), so fix before Phase 3.4 wires it. | todo |
| C6 | M | `storage.rs:251` | State read is now `openat(O_NOFOLLOW)` + `validate_private_regular_file` + `fchmod 0600` — but `read_to_end` has **no size cap**, so a large regular state file still OOMs. Port only the cap. | todo |
| C7 | M | `git.rs:6-26`, called `runtime.rs:299` | Still `Command::new("git").arg("-C").arg(cwd)` every 2 s per workspace, with no `GIT_CONFIG_NOSYSTEM` / ceiling / `--git-dir` pin — so a hostile repo's `.git/config` (`include.path`, `core.fsmonitor`) is parsed merely by opening the workspace. v1's `.git/HEAD` reader ports, but into `git::current_branch` directly (no `BranchProbe` trait here). | todo |
| C8 | M | `pty.rs:2313-2329,2362-2379` | `server_executable` resolves by filename next to `current_exe()` with no owner/mode check; `spawn_server` has no `env_clear()`, so the long-lived daemon inherits the first client's full environment (API keys) and passes it to every later client's PTYs. | todo |
| C9 | M | `pty.rs:432-457,1950-1977` | Terminal query responses accumulate uncapped and are emitted one socket write each, on the render thread — ~2048 writes per 8 KiB of `\x1b[6n`. | todo |
| C13 | L | `runtime.rs:553,561,647-655` | OSC 52 still written straight to `io::stdout()` outside the ratatui frame on every mouse-up, no opt-out, no tmux passthrough. | todo |
| C14 | L | `pty.rs:460-463` | `append_terminal_system_line` feeds server-supplied text (`PtyEvent::Error`, `ExitInfo::signal`) to `parser.process()` with no control-byte stripping — UI spoofing within the emulator. | todo |
| S6 | L | `storage.rs:807-829` | **New.** `validate_private_regular_file` checks regular/owner/`nlink` but not `mode & 0o077`, while its sibling `read_mult_agent_status_records` (`runtime.rs:2222`) does. One-syscall window (it `fchmod`s right after), but close the inconsistency. | todo |
| S7 | L | `paths.rs:38-40` | **New.** `resolve_user_base` accepts any *absolute* `$XDG_CONFIG_HOME`/`$XDG_DATA_HOME` with no ownership check. Harmless for state (re-validated by `SecureDirectory`) but it makes the config path attacker-steerable via a second env var. Closed by C2. | todo |
| S8 | L | `mult-claude-status.sh:36`; `mult-status.ts:33-48` | **New.** The C5 remediation dropped `O_NOFOLLOW` discipline along with the temp file: `[ -f ]`/`statSync` follow symlinks and append through them. Only reachable inside a directory already certified 0700-and-owned, so low. | todo |

**Fixed by `main`:** C1 (never relaunches persisted `Command`; better than v1's), C4 (status path
fails closed), C10 (content-addressed `-v2` runtime artifacts + rotation + removal on exit),
C12 (`SessionIdentity` capability gate). **Obsolete:** C5, C11 (the predictable-temp-file and
0755-mkdir schemes no longer exist; the extensions now append to a file `mult` pre-creates).

## D. Performance

| ID | Sev | Where | Item | Status |
|----|-----|-------|------|--------|
| D1 | H | `runtime.rs:1823-1853`; `pty.rs:867-891` | **Worse than v1:** `changed` is computed for the return value only, and `resize` is called unconditionally — now from **two** sites (terminal *and* chat agent), so a `Resize` is serialized and written to the socket 125×/s at idle. One-line gate at both. | todo |
| S3 | M | `runtime.rs:2141-2148` | **New, and now the largest idle cost.** The status bridge runs every 16 ms tick per agent chat: `format!` + 2 `join`s + a `PathBuf` clone (4 allocations) then `open`+`fstat`+`seek`+`read`+`close`. With 4 chats ≈ 1250 syscalls/s and ~1000 allocations/s at idle. (Same root as B3.) | todo |
| D2 | H | `ui.rs:784,805-889` | `TerminalScreen::from_vt100` deep-copies every cell per frame into `Vec<TerminalCell>` with `symbol: String`, and `ui.rs:881` calls `.to_string()` on `contents()`, which already returns an owned `String`. Blank cells still call `contents()`. v1 measured 414 µs / 20 000 allocs per frame. | todo |
| D3 | H | `ui.rs:708-711`; `app.rs:1082,1139` | `terminal_all_lines` (a `String` per row) is an eager argument scraped every frame, but the callee returns `None` immediately when no search is active. Same shape for chats. | todo |
| D4 | M | `runtime.rs:44-46,219-271` | `main` added `needs_redraw` gating (real improvement), but the 16 ms tick still runs D1 ×2, S3, both drains, `save_if_dirty_with` and two workspace scans. The wakeup-source half is still open. | todo |
| S4 | L | `app.rs:326,338`; `runtime.rs:228-230` | **New.** `replace_workspace_git_branches` returns `bool`, but the caller discards it and sets `needs_redraw = true` unconditionally — defeating `main`'s own redraw gating twice a minute at idle. | todo |
| D5 | M | `git.rs:11`; `runtime.rs:227,293` | `git` forked synchronously on the UI thread every 2 s per workspace. Mostly resolved by C7's `.git/HEAD` reader. | todo |
| D6 | M | `pty.rs:184,1903,1914` | `TerminalResponseState::Csi(Vec<u8>)` heap-allocates on entering every CSI sequence; the 128-byte bound already exists, so the inline-array fix is a drop-in. | todo |
| D7 | M | `pty.rs:1950-1977` | Every escape-sequence byte still takes `parser.process(from_ref(&byte))`. **Note:** `main` now *documents* the per-byte feed as intentional (`pty.rs:1944-1949`), so any fix must preserve the CPR carve-out and update that comment, or it reads as a regression. Requires G4 first. | todo |
| S5 | L | `runtime.rs:1725` | **New.** `write_private_runtime_file` takes a *blocking* `flock(LOCK_EX)` on a fixed path from the UI thread; a second `mult` starting an agent stalls the first one's render loop. Use `LOCK_EX\|LOCK_NB` + retry. | todo |
| D8 | L | `ui.rs:464-489` | `truncate_text` does `ch.to_string()` + `Span::raw` per character. | todo |
| D9 | L | `ui.rs:63-78,153` | `Palette::from_colorscheme` re-parses 12 hex strings every frame; no memo. | todo |
| D10 | L | `pty.rs:48-52,1103,1350`; `runtime.rs:1885` | `PtyEvent::Output`/`Scrollback` carry full payloads through the queue to an empty match arm. | todo |
| D11 | L | `pty.rs:142` | `SERVER_EVENT_QUEUE_CAPACITY = 4096` × 8 KiB ≈ 32 MiB client-side backlog if the UI thread stalls. | todo |

## E. UX, CLI & configuration

| ID | Sev | Where | Item | Status |
|----|-----|-------|------|--------|
| E1 | H | `main.rs:18`; `mult-server.rs:206` | No argv handling in either binary; no `std::env::args`, no `default-run`. `mult --version` launches the TUI. | todo |
| E2 | H | `pty.rs:213,1398,1489` | Daemon-connection failure still queued at `PtyKey::Terminal(TerminalId(0))`, which cannot exist. No status/notice surface in `app.rs`. Blocks B8. | todo |
| E5 | H | `config.rs:141,219`; `main.rs:30` | Bad config still `Debug`-dumps with no filename. `main` returns `io::Result`, so v1's `ExitCode` fix needs adapting. | todo |
| E4 | H | `ui.rs:1732` | No in-app help; `keybinding_help_line_is_not_rendered` still asserts the footer's absence; no shared binding table. | todo |
| E6 | M | `config.rs:13-31` | No `deny_unknown_fields`, no colour-parse reporting, no project-path validation. | todo |
| E11 | M | `storage.rs:56,185,255`; `model.rs` | `main`'s backup mechanics are better than v1's (`SecureDirectory`, `.corrupt-*`, 0600, V1→V2 migration) — but `LoadedState` carries **no user notice**, so the user is still never told, and only 4 `#[serde(default)]` exist, so one renamed/`null` required field still discards every workspace. Port the lenient-decode + notice halves only. | todo |
| E7 | M | `app.rs:1375-1412` | Prompt input still append-only across four duplicated arms; no cursor. | todo |
| E8 | M | `ui.rs:310` | Chat state carried by colour alone (`"● "` for every state). | todo |
| E12 | M | `README.md:147`; `docs/DAEMON.md:147`; `ui.rs:610` | **New (F1 residual).** `MULT_AGENT_CMD` is documented as a live knob but is inert, and chat search silently searches an always-empty transcript. Mark experimental/no-op — do **not** remove the code. | todo |
| E9 | L | `app.rs:158-175`; `main.rs:35` | No config reload; `config` moved into `runtime::run`. | todo |
| E10 | L | `ui.rs` | `NO_COLOR` appears nowhere in the repo. | todo |

**Fixed by `main`:** E3 (`Prompt::ConfirmDelete` — and `main` always prompts, with no empty-item
skip, so it is strictly safer than v1's).

## F. Architecture

| ID | Sev | Where | Item | Status |
|----|-----|-------|------|--------|
| F1 | — | — | **ABANDONED.** See "Decisions recorded". Residual is E12 only. | dropped |
| F2 | H | `pty.rs:150-190,393,407` | **Worse than v1:** `PtyRuntime` now has **12** parallel maps keyed by `PtyKey`/`PaneId` (was 8) — `pane_leases`, `expected_output`, `session_identities`, `agent_sessions` were added. Every lifecycle op must touch the right subset. | todo |
| F7 | H | `pty.rs:2214-2263` vs `mult-server.rs:706-748` | **Worse than v1:** peer check still byte-duplicated, and `current_euid` now exists in **three** places plus three inline `libc::geteuid()` and one in protocol. No `crates/protocol/src/peer.rs`. Blocks C3. | todo |
| F9 | M | `main.rs:13`; `lib.rs` | `runtime` is still binary-private and 3904 lines, fusing event loop + keymap + mouse + clipboard + hook generation + status polling + git + save. Tests cannot reach it. | todo |
| F15 | M | `ui.rs` (2272 lines) | Contrast maths + vt100 adapter + all widgets still fused. | todo |
| F5 | M | `app.rs:23-24,316,605` (2614 lines) | `prompt: Option<Prompt>` and `focus: FocusMode` still orthogonal; `sync_focus_to_selection` hand-sprinkled at three sites. | todo |
| F6 | M | `ui.rs:152,177,182,186` | No `src/layout.rs`; `layout_areas` recomputed at three call sites per iteration; renderer is still the geometry oracle. | todo |
| F3 | M | `pty.rs:192,203,1556`; `runtime.rs:202` | **Premise changed:** `impl Default for PtyRuntime` not only still exists, it is the *production* constructor — so the `SpawnPolicy` fix must also replace that call site, which v1's diff does not. | todo |
| F12 | M | `model.rs:285-330`; `storage.rs:218` | `try_default_with` still seeds a `"website"` demo workspace, and the corrupt-recovery path still hands it to the user. | todo |
| F16 | M | `model.rs:272`; `app.rs:33,636,957` | Persisted `TerminalStatus` still duelling with `PtyRuntime::is_running`; `seen_done` side table intact. **Bigger than v1:** must now coexist with `STATE_VERSION = 2`, its V1→V2 migration, and the future-version byte-preservation guarantee. | todo |
| F8 | M | `protocol/lib.rs`; `mult-server.rs:3545` | No `RejectCode`; `io::Error` still universal. The `"already attached"` match is gone, but text matching remains (`"lease space exhausted"`). Must land in the same wire bump as any other protocol change. | todo |
| F10 | M | `runtime.rs:63` | No `BranchProbe` / `AgentStatusSource` seams; the new `AgentStatusBridgeState` is concrete and file-backed, so its tests hit the real filesystem. | todo |
| F13 | M | `app.rs:582,744,766,1259` | Four copies of the wrap-around list-selection body; no shared prompt-key handler. | todo |
| F14 | M | `ui.rs:265,348`; `app.rs:795` | `sidebar_selected_index` still re-walks the order `nav_iter` claims to own. | todo |
| F17 | L | various | Most of the dead-surface list stands (`ensure_parser`, `scroll_to_top/bottom`, `search_status`, `focus_next`, `pane_inner`, `output_area_after_header` with both header constants `0`, and 4 unused `ClientMessage` variants). **Exceptions: `terminal_all_lines` is now live** (`ui.rs:710`), and `SessionInfo`/`PaneInfo`/`Sessions` are live. | todo |
| F18 | L | `ui.rs:642,650-652` | Every agent kind still renders "Pi agent not started" and the wrong config keys. | todo |
| F20 | L | various | `crates/protocol/src/` is `lib.rs` only. `invalid_data` ×4, `shell_command_args`+`default_shell` ×2, `shell_quote` vs `shell_display_arg`. **The palette half regressed** — `Color::Rgb` constants are back in `ui.rs:34-43` with no `DEFAULT_COLOR_SCHEME`. `random_u64` is now single-source (that sub-item is fixed). | todo |
| F19 | L | `app.rs:605/609,787/1075,1322/1326` | **Scope reduced:** the full `terminal`→`pty` rename now conflicts with ~8k lines of rewritten code for little gain. Reduce to deleting the three identical alias pairs, which is conflict-free. | todo |
| F21 | L | `app.rs:744,766` | Non-modular wrap in the list-step body, twice. Fold into F13. | todo |

## G. Tests

| ID | Sev | Where | Item | Status |
|----|-----|-------|------|--------|
| S1 | M | `transcript.rs`; `lib.rs:9` | **New.** `TranscriptJournal` has zero call sites outside its own tests — 344 lines of file-truncating I/O shipped in the binary, exercised only by tests. Intentional per the roadmap, but it needs S2 fixed and a note that it is unwired. | todo |
| G4 | H | `pty.rs:1949-1950` | The "behaviourally identical to feeding every byte individually" claim is still untested. Blocks D7. No `proptest` here — hand-rolled seeded generator. | todo |
| G3 | H | no tracked `fuzz/` | No fuzz targets. v1's `vt_response_detector` found A13 on its first run, so the value is proven. **Note:** an untracked `fuzz/` of build residue is currently polluting `git status`. | todo |
| G6 | M | `ui.rs:2151-2168` | `draw_text` now dumps the whole buffer (better than v1's per-row grep) but every assertion is `.contains(...)`. Zero full-buffer equality, no 80×24 case. | todo |
| G2 | M | `protocol/lib.rs:727,856-940` | Framing tests cover oversize and trailing bytes; still untested: truncated frame, `len == 0`, payload split across reads, malformed postcard. No `crates/protocol/tests/`. | todo |
| G5 | M | `storage.rs:381,900+` | Storage tests grew a lot, but no serde *shape*-error test and no backup-rename-failure test. | todo |
| G9 | M | `mult-server.rs:51,1573` | `RAW_HISTORY_CHUNK_BYTES` is referenced by no test; the largest replay fixture is ~416 bytes — one chunk. Boundary cases untested. | todo |
| G7 | M | `protocol/lib.rs:1437,1441`; `config.rs:248` | Two of the three env-mutating tests remain, including `set_var` on a process global raced by sibling tests. | todo |
| G11 | M | `pty_integration.rs:34,163+`; `agent.rs:435,499` | Embedded `sleep 1/2/3` under a 5 s cap; 2 s deadline polled at 10 ms. | todo |
| G10 | M | `agent.rs:370-503` | Still exactly the 4 audited happy-path tests. | todo |
| G12 | M | `ui.rs:821,865,921` | vt100→ratatui adapter untested directly; no wide-cell case. | todo |
| G13 | L | `config.rs:224-368` | 9 tests, none for malformed JSON, unknown keys, or bad hex. Policy is settled in code, so these can be written directly. | todo |
| G14 | L | `git.rs:6,37-52` | `current_branch` untested; only `first_nonempty_line` covered. | todo |
| S9 | M | `flake.nix:30` | **New.** The `nix` CI job sets `MULT_SKIP_PTY_INTEGRATION=1`, so it reports 24 tests in 0.00 s — a fully green job with zero PTY coverage, and nothing anywhere asserts the integration tests actually ran. This is the unmet half of v1's G1. | todo |
| S10 | L | `flake.nix:27` | **New.** The flake sets `MULT_TEST_SHELL`, which only the skipped integration file reads, while the code that actually needs a shell in the sandbox (`mult-server.rs:2775`, `pty.rs:2487`) reads `$SHELL`, which the flake does not set. Latent trap for the next test that spawns a pane. | todo |

**Fixed by `main`:** G1 (**verified empirically** — the harness returns `Result`, all 24 call sites
`.expect()`, and a sabotaged setup fails red), G8 (~33 server tests drive `handle_client` over a
real `UnixStream::pair`), G15 (`ensure_private_dir` breaks at the filesystem root; `nix flake
check` passes).

## H. CI, tooling, docs & release

| ID | Sev | Where | Item | Status |
|----|-----|-------|------|--------|
| S11 | M | `.github/workflows/ci.yml:44` | **New.** The MSRV job runs a bare `cargo check` while `rust-toolchain.toml` pins `1.94` — and the toolchain file beats `rustup default`. Whether the job tests 1.88 at all depends on `dtolnay/rust-toolchain` exporting `RUSTUP_TOOLCHAIN`. Add an explicit `+1.88`. (1.88 does currently compile — verified.) | todo |
| H3 | M | `justfile:37`; `README.md:67`; `CONTRIBUTING.md:26` | `just ci` = `fmt-check lint test audit`; CI additionally runs macOS, `cargo deny`, an npm typecheck, MSRV, and `nix flake check`. Both docs still misdescribe it. | todo |
| H4 | M | `CONTRIBUTING.md:10,14` vs `flake.nix:60-68` | CONTRIBUTING promises `cargo-deny` from `nix develop`; the devShell has `cargo-audit` but not `cargo-deny`. | todo |
| H5 | M | `.github/workflows/` | No release workflow, no tags, no binaries. | todo |
| H7 | M | `README.md:129` vs `config.rs:40-65` | 3 of 12 colorscheme keys documented; `_nc` still unguessable. No `docs/CONFIG.md`. | todo |
| H8 | M | `docs/` | No troubleshooting guide. Messages must be re-quoted from current source. | todo |
| H9 | M | `ci.yml`; `justfile` | No coverage measurement over ~19k lines. | todo |
| H10 | L | `ci.yml:3-6,29,60` | No `schedule:`/`workflow_dispatch`; `cargo audit` and `cargo deny check advisories` still both present and redundant. | todo |
| H11 | L | `.github/` | No issue/PR templates, no CODEOWNERS. | todo |
| H12 | L | `CHANGELOG.md` | No reference-link definitions, so both headings render literally; `[0.1.0]` undated; no tag. | todo |
| H13 | L | `README.md:170-183`; `extensions/package.json:6` | Layout table omits `runtime.rs`, `git.rs`, `paths.rs`, `lib.rs`, `terminal_guard.rs`, `transcript.rs`; package.json still says the extension is embedded in `src/main.rs` (it is `runtime.rs:73`). | todo |
| H14 | L | `CONTRIBUTING.md` | `MULT_SKIP_PTY_INTEGRATION` and `MULT_TEST_SHELL` documented nowhere. | todo |
| H15 | L | `.gitignore` | Missing `.claude/settings.local.json`, `*.corrupt-*`, editor dirs, and now `/fuzz/target`, `/fuzz/corpus`, `/fuzz/artifacts`. | todo |
| H16 | L | `justfile` | No `install-hooks` recipe. | todo |
| H17 | L | 4 files | Four independent `0.1.0` literals; no `[workspace.package] version`, no `just version-check`, no `docs/RELEASING.md`. | todo |
| S12 | L | `docs/` | **New.** `README.md:196` and `CONTRIBUTING.md:44` point at `docs/REMAINING_WORK.md` as the authoritative follow-up list, but it is a stale "Phase 3 partially implemented" handoff plan, and this backlog is linked from nowhere. Reconcile the two into one roadmap. | todo |

**Fixed by `main`:** H1 (MSRV job exists — see S11 for the caveat), H2 (`--locked` everywhere),
H6 (`docs/REMAINING_WORK.md` exists, so the dangling links resolve).
