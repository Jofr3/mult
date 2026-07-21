# Remaining improvement plan

This document is the handoff plan for work remaining after the Phase 1 safety pass. It is intentionally split into reviewable phases so a new coding session can implement one phase without redesigning the whole project.

## Current handoff state

Phases 1 and 2 are implemented. Phase 2 is present in the current working tree. Phase 3 is **partially implemented** on top of it and upgrades the wire protocol to version 10 and durable state to version 2. Remaining Phase 3 blockers are production end-to-end status-bridge reconnect/finalization coverage and authoritative normal Pi/Claude message boundaries with transcript-journal integration; do not claim Phase 3 complete until the deferred checklist and restart tests below are complete. Before continuing, inspect `git status` and `git diff`; do not revert or independently reimplement the existing work.

Phase 1 includes:

- attach-only restoration for persisted command terminals;
- terminal-mode RAII cleanup and graceful signal handling;
- future-state envelope checks and bounded ID allocation;
- deletion confirmation and acknowledged PTY stops;
- nonfatal save failures with retry behavior;
- bounded PTY integration-test setup and diagnostics;
- pinned CI actions/tools, locked Cargo commands, npm Dependabot, and a Rust 1.88 MSRV lane;
- protocol version 8 with pane-correlated `StopResult`;
- `anyhow` 1.0.103 for `RUSTSEC-2026-0190`.

Phase 2 additionally includes:

- correlated, resumable, idempotent create/attach/stop requests;
- connection-bound attachment leases with structured takeover;
- sequence-numbered, truncation-aware attach replay ordered before live output;
- exactly-once child finalization and process-group termination;
- explicit uncertain-delivery handling that never replays input or paste;
- deterministic protocol, ownership, replay, shutdown, and lifecycle regression tests.

The Phase 1 baseline validation passed:

```sh
nix develop -c just ci
nix shell nixpkgs#cargo-deny -c cargo deny --locked check
rustup run 1.88.0 cargo check --locked --workspace --all-targets --all-features
(cd extensions && npm ci --ignore-scripts && npm run typecheck)
nix shell nixpkgs#actionlint -c actionlint .github/workflows/ci.yml
nix flake check "path:$PWD"
```

The current Phase 3 partial implementation was validated with:

```sh
nix develop -c just ci
nix shell nixpkgs#cargo-deny -c cargo deny --locked check
rustup run 1.88.0 cargo check --locked --workspace --all-targets --all-features
(cd extensions && npm ci --ignore-scripts && npm run typecheck)
nix shell nixpkgs#actionlint -c actionlint .github/workflows/ci.yml
nix flake check "path:$PWD"
```

Known non-blocking dependency warnings remain:

- cargo-deny reports existing duplicate dependency families and unmatched license allowances;
- npm reports four extension dependency vulnerabilities (one moderate, three high) and deprecated upstream Pi package names.

## Rules for every phase

1. Read `AGENTS.md` and `README.md` completely before editing.
2. Read `docs/DAEMON.md` completely for protocol, socket, daemon, or PTY work.
3. Preserve command execution semantics:
   - `MULT_AGENT_CMD` is parsed into argv and does **not** use a shell;
   - `pi_agent_command`, `claude_code_command`, and `TerminalLaunch::Command` intentionally use `$SHELL -lc`.
4. Keep protocol changes versioned and update both client and daemon in the same change.
5. Add deterministic regression tests for each fixed race or failure path.
6. Keep state, runtime files, and IPC paths owner-private.
7. End each phase with formatting, locked tests, clippy, audit, cargo-deny, MSRV, extension typechecking, actionlint, and Nix checks when available.
8. Prefer one phase per pull request. Do not mix UX redesign with daemon lifecycle changes.

## Recommended order

```text
Phase 2: IPC and PTY lifecycle correctness
    ↓
Phase 3: durable ownership, identity, status, and transcripts
    ↓
Phase 4: resource bounds and performance
    ↓
Phase 5: terminal input UX and accessibility
    ↓
Phase 6: compatibility, testing, dependencies, and release hygiene
    ↓
Phase 7: optional larger architecture
```

Phases 4 and 5 may proceed independently after Phase 2 if their files are kept separate. Phase 3 should settle state/session identity before implementing long-term transcript or snapshot storage.

---

# Phase 2 — IPC and PTY lifecycle correctness

**Status:** completed in protocol version 9. The details below are retained as the implementation and regression-test checklist.

## Goal

Make daemon operations correlated, ordered, ownership-aware, idempotent where possible, and finalized exactly once.

## Primary files

- `crates/protocol/src/lib.rs`
- `src/pty.rs`
- `src/bin/mult-server.rs`
- `tests/pty_integration.rs`
- `docs/DAEMON.md`

## Work packages

### 2.1 Correlate every stateful request

Phase 1 added a pane-correlated `StopResult`, but create and attach still rely on generic responses and errors.

- Add a bounded `RequestId` to `CreateSession`, `Attach`, and `Stop`.
- Add structured `CreateResult`, `AttachResult`, and `StopResult` responses carrying the same request ID.
- Replace generic errors for these operations with scoped result errors.
- Keep generic `ServerMessage::Error` only for connection/protocol-level failures.
- Ensure unrelated output or an error for pane A cannot complete or abort a pending request for pane B.
- Define retry behavior explicitly:
  - create: idempotent for the same request/session identity;
  - attach: idempotent;
  - stop: idempotent, with “already absent” treated as confirmed success;
  - raw input/paste: never silently replay after uncertain delivery.
- Bump `PROTOCOL_VERSION` and document the compatibility break.

### 2.2 Add attachment ownership leases

Current takeover removes the old subscriber but does not make the old client incapable of mutation.

- Give each successful attachment an opaque lease/generation token.
- Require the current lease on input, paste, resize, detach, and stop requests.
- Invalidate the previous lease during takeover.
- Send a structured `TakenOver` event to the displaced client.
- Reject stale-lease mutations without closing the entire connection.
- Decide whether one connection may own multiple panes and model leases accordingly.

### 2.3 Order attach replay and live output

A client must never observe live output before attachment acknowledgement and historical replay.

- Introduce an attach barrier or snapshot sequence:
  1. attach accepted;
  2. snapshot/replay begin;
  3. replay chunks;
  4. replay end with a watermark;
  5. live output after the watermark.
- Add monotonically increasing per-pane output sequence numbers if needed.
- Do not publish the client as a live subscriber until the replay boundary is established.
- Specify how history truncation is reported.

### 2.4 Centralize child ownership and finalization

Manual stop and natural exit currently compete for the child handle.

- Give exactly one waiter ownership of each child lifecycle.
- Represent lifecycle explicitly, for example `Running → Stopping → Exited → Removed`.
- Route natural exit, requested stop, spawn failure, kill failure, wait failure, and daemon shutdown through one exactly-once finalizer.
- Emit one definitive final result/event.
- Remove the session exactly once.
- Treat “already exited” as successful cleanup.
- Preserve a retryable/recoverable state when kill or wait genuinely fails.

### 2.5 Stop process groups, not only the direct shell

Configured shell commands may launch pipelines or descendants.

- Track the PTY child process group/session.
- Send SIGTERM to the group.
- Wait for a bounded grace period.
- Escalate to SIGKILL when needed.
- Reap the direct child exactly once.
- Keep platform-specific behavior isolated and tested.

### 2.6 Make uncertain delivery explicit

The client currently reconnects and may replay a message after a failed write.

- Automatically retry only requests proven idempotent by request ID.
- Never silently duplicate terminal input or paste.
- Return a distinct “delivery uncertain” error when a disconnect occurs after partial/unknown delivery.
- Keep local attachment state until a correlated response or a later reconciliation proves the server state.

## Required tests

- Interleave create/attach/stop responses for multiple panes and verify correct correlation.
- Error pane A while pane B attaches successfully.
- Take over a pane and verify the old client cannot input, paste, resize, detach, or stop it.
- Continuously emit numbered output during attach and assert exact ordering, no gaps, and no duplication.
- Race natural exit against stop repeatedly and assert one final event and no stale session.
- Stop a pipeline and a descendant that ignores SIGTERM; assert no descendant survives.
- Simulate a disconnect after possible input delivery and assert no automatic duplicate write.
- Add exact protocol round-trip fixtures for every new message.

## Completion criteria

- All stateful operations have request correlation.
- Takeover is an enforceable ownership boundary.
- Attach replay has a documented ordering guarantee.
- Every child is finalized and reaped exactly once.
- Input is never blindly replayed after uncertain delivery.
- Full local and integration gates pass without sleeps used as correctness synchronization.

---

# Phase 3 — Durable ownership, session identity, status, and transcripts

**Status: partial.** Work packages 3.1, 3.2, 3.5, and 3.6 are implemented in the current working tree. Package 3.3 has production plumbing and daemon-level regression coverage but still needs an end-to-end bridge reconnect/finalization test and tombstone-recovery cleanup coverage. Package 3.4 has a bounded journal codec but remains deferred at the production-capture boundary.

Implemented foundation checklist:

- [x] Process-lifetime owner lock acquired before load and retained through TUI/terminal cleanup; all runtime saves target that locked directory and unlocked library saves must acquire the same lock.
- [x] State schema v2 with a random persistent namespace, immutable per-chat/terminal tokens, explicit V1 migration, `needs_save`, exact migration fixtures, safe modes, and future-version byte preservation.
- [x] Protocol v10 identity on namespaced list/create/attach/stop, local rejection of unregistered production operations, daemon identity indexes/checks, and attach-only restoration without command relaunch.
- [x] Persisted random agent generation saved before launch; real Pi/Claude bridges emit PID-independent append-only identity-complete records; client and daemon validate schema/chat/backend/identity/generation; daemon final status is reconnectable and monotonic.
- [x] Private, bounded status files, fixed versioned generated bridge artifacts, observed-finalization cleanup, and stale generation count rotation.
- [x] HOME/XDG resolution uses absolute XDG/HOME, then the effective user's passwd home, and otherwise fails instead of selecting the current project directory.
- [x] Separate bounded append-only transcript primitives validate identity/order and recover a truncated final record without scraping PTY bytes.

Deferred acceptance checklist and missing completion tests:

- [ ] Exercise the production file → TUI → daemon status bridge across disconnect/reconnect and finalization. When only a recovered daemon tombstone proves finality, remove the obsolete journal or explicitly retain it for bounded rotation.
- [ ] Define and implement authoritative structured user/assistant/tool message boundaries for normal Pi and Claude Code sessions. Lifecycle hooks and arbitrary PTY read chunks are not message boundaries.
- [ ] Append those structured events to the transcript journal, hydrate ordered search from it, and define retention/compaction behavior without silently losing records.
- [ ] Persist normal Pi/Claude chat output, restart both daemon and TUI, and verify ordered searchable recovery. Until this test exists, normal Pi/Claude PTY output is documented as daemon-lifetime raw history only.

## Goal

Prevent state conflicts and identity collisions, and make restored status/transcript behavior match the persistence promises.

## Primary files

- `src/model.rs`
- `src/storage.rs`
- `src/app.rs`
- `src/runtime.rs`
- `src/pty.rs`
- `src/bin/mult-server.rs`
- `crates/protocol/src/lib.rs`
- `README.md`
- `docs/DAEMON.md`

## Work packages

### 3.1 Choose an authoritative state ownership model

Current atomic rename prevents partial writes but not concurrent last-writer-wins loss.

Choose and document one model:

**Preferred near-term option:** one process-lifetime state lock.

- Acquire an owner-private lock before loading mutable state.
- Fail clearly when another TUI owns the state.
- Hold the lock for the full process lifetime.
- Ensure panic and signal cleanup release it.

**Alternative:** revision/CAS storage.

- Add a monotonically increasing revision.
- Reject a save based on a stale revision.
- Present a conflict instead of overwriting another process.

Do not use only a short per-save lock; that still permits stale snapshots.

### 3.2 Namespace durable state and daemon sessions

Raw numeric IDs can collide after state loss, alternate state paths, or copied state.

- Add a random, persistent state namespace UUID/token.
- Give each daemon session an immutable logical session token.
- Include namespace and token in create, list, attach, and stop operations.
- Verify identity before attaching to an existing numeric session.
- Migrate existing state without relaunching commands.
- Reject a session from another state namespace even when the numeric ID matches.

### 3.3 Replace PID-scoped status files

Agent status files are tied to a TUI PID and are not authoritative after reconnect.

- Prefer daemon/session-generation-scoped status IPC.
- Include chat ID, agent kind, session token, generation, and schema version.
- Reject stale or wrong-chat updates.
- Ensure final failed/exited status cannot be overwritten by a late `running` update.
- Clean generated status files/extensions/settings on normal exit and through bounded stale-file rotation.
- If files remain as a transition mechanism, validate ownership, regular-file status, size, generation, and liveness.

### 3.4 Make transcript persistence truthful and durable

Normal Pi/Claude PTY output is not currently persisted as a structured chat transcript.

- Decide what constitutes an authoritative chat message boundary.
- Prefer structured agent events over scraping arbitrary terminal bytes.
- Store transcript events separately from compact workspace metadata.
- Use append-oriented storage with bounded record and file sizes.
- Recover from a truncated final record without losing earlier messages.
- Define retention, compaction, and search behavior.
- Until complete, keep README wording precise about what survives daemon and client loss.

### 3.5 Complete explicit schema migrations

- Centralize schema migrations in storage rather than mutating versions in multiple layers.
- Return `needs_save` after migration.
- Keep golden fixtures for every historical schema.
- Preserve future-version files byte-for-byte.
- Normalize existing state/backup ownership and mode to `0600` where safe.

### 3.6 Resolve fallback directory behavior

When neither `HOME` nor the relevant `XDG_*` variable exists, state/config can land in the current working directory.

- Prefer a private per-UID fallback directory or a clear startup error.
- Never silently put state in an arbitrary project directory.
- Retain owner-only permissions and symlink protections.

## Required tests

- Two clients load revision N and attempt divergent saves; assert locking or conflict detection.
- Start a daemon session, switch/reset state, reuse its numeric ID, and assert attach rejection.
- Send wrong-version, wrong-chat, stale-generation, and late status events and assert rejection.
- Crash after a final status transition and ensure it remains authoritative.
- Persist chat output, restart both daemon and TUI, and verify ordered searchable transcript recovery.
- Truncate the final transcript record and verify prior records remain readable.
- Run golden migrations for each supported state version.

## Completion criteria

- Concurrent state writers cannot silently lose data.
- Session identity survives numeric ID reuse safely.
- Agent status is reconnectable and generation-safe.
- Documented transcript persistence matches observed behavior.
- All migrations are explicit and fixture-tested.

---

# Phase 4 — Resource bounds and performance

## Goal

Keep memory, disk, CPU, and UI latency bounded under hostile or accidental output floods.

## Primary files

- `src/pty.rs`
- `src/bin/mult-server.rs`
- `src/runtime.rs`
- `src/storage.rs`
- `src/config.rs`
- `src/git.rs`
- `src/ui.rs`
- `crates/protocol/src/lib.rs`
- `Cargo.toml`

## Work packages

### 4.1 Bound terminal control-sequence parsing

The current VTE stack can accumulate an unterminated OSC sequence independently of scrollback limits.

- Evaluate a maintained parser with explicit sequence limits.
- Otherwise add a strict control-string byte budget with reset/discard behavior.
- Bound OSC, DCS, APC, PM, SOS, and CSI state independently.
- Ensure normal parsing resumes after an oversized sequence.
- Do not rely on the visible scrollback cap for parser-state memory safety.

### 4.2 Replace quadratic history eviction

- Replace the daemon’s front-drained `Vec<u8>` with `VecDeque`, chunks, or a byte ring.
- Preserve the newest bounded suffix efficiently.
- Avoid copying the complete history for each attach where possible.
- Track truncation explicitly for replay consumers.

### 4.3 Move socket I/O off the TUI thread

- Give the connection one owned I/O worker.
- Use byte-bounded outbound queues, not only message-count bounds.
- Add write deadlines and explicit socket shutdown.
- Keep a join handle and bounded shutdown path.
- Avoid writes in `Drop`.
- Coalesce redundant resize/activity messages.

### 4.4 Add daemon-wide quotas

Add configurable or documented hard limits for:

- concurrent clients;
- sessions and PTYs;
- sessions per client/UID;
- aggregate history bytes;
- queued outbound bytes;
- request rate and idle connections;
- frame read deadlines, not only frame size.

Reject excess work gracefully without crashing or blocking existing sessions.

### 4.5 Budget event-loop work

- Add per-tick byte, message, and time budgets.
- Avoid returning raw output bytes after they have already been parsed when the runtime only needs an activity signal.
- Deduplicate unchanged resize requests.
- Coalesce status updates.
- Preserve keyboard/render responsiveness during sustained output.

### 4.6 Bound persisted and configured input

Define limits for:

- complete state file size;
- config file size;
- number of workspaces/chats/terminals;
- transcript records and message length;
- partial lines without newline;
- paste/input payload size;
- command-tracker buffer length;
- generated runtime files.

Surface truncation/rejection visibly rather than silently consuming unbounded data.

### 4.7 Debounce and restructure persistence

- Debounce metadata snapshots during output-heavy sessions.
- Flush on controlled shutdown and important state transitions.
- Keep transcripts append-oriented.
- Measure bytes written and fsync frequency.

### 4.8 Move Git probing to a bounded worker

- Run Git queries outside the UI thread.
- Apply a timeout and latest-value-wins queue.
- Cancel or discard stale refreshes.
- Keep invocation shell-free.

### 4.9 Reduce render allocations

- Avoid rebuilding and duplicating every visible terminal cell string on each redraw.
- Add dirty-region or borrowed-cell rendering where supported.
- Benchmark common and large-pane redraws before and after optimization.

## Required tests and benchmarks

- Multi-megabyte unterminated OSC/DCS input remains bounded and parsing recovers.
- History property tests verify exact retained suffix and cap behavior.
- A peer that stops reading cannot freeze input or terminal cleanup.
- Output floods retain bounded input/render latency.
- Quota exhaustion rejects only the excess client/session.
- Oversized config/state/paste/transcript input fails with useful diagnostics.
- Slow Git commands cannot stall input or redraw.
- Record memory, allocations, fsync count, and redraw latency baselines.

## Completion criteria

- Every externally influenced buffer has a documented bound.
- No socket or Git operation can block the TUI thread indefinitely.
- History eviction is amortized O(1).
- Flood tests demonstrate bounded memory and interactive latency.

---

# Phase 5 — Terminal input UX and accessibility

## Goal

Let terminal applications receive expected input while keeping multiplexer controls discoverable and accessible without relying only on color or mouse input.

## Primary files

- `src/runtime.rs`
- `src/app.rs`
- `src/ui.rs`
- `src/pty.rs`
- `src/config.rs`
- `README.md`

## Work packages

### 5.1 Introduce command and passthrough modes

Global shortcuts currently consume common shell/readline keys.

- Add an explicit multiplexer leader key or command/passthrough mode.
- In passthrough mode, deliver ordinary control keys to the child.
- Make mode and focus visibly obvious with text/symbols, not color alone.
- Make the leader/keymap configurable with conflict validation.
- Preserve emergency quit and terminal recovery paths.

### 5.2 Forward complete mouse protocols

- Forward click, release, drag, motion, wheel, and modifiers when the child enables mouse reporting.
- Support the terminal protocols exposed by the parser.
- Reserve a documented modifier, such as Shift, for local selection.
- Keep local behavior when the child has not enabled mouse reporting.

### 5.3 Add keyboard-only scroll, selection, and copy

- Add line/page scroll, top, bottom, selection start/extend, copy visible selection, and clear selection actions.
- Expose commands through the command palette and help/documentation.
- Ensure operation without mouse capture.

### 5.4 Build a grapheme-aware prompt editor

- Track a cursor rather than append-only input.
- Support Left/Right, Home/End, Delete, word movement, and horizontal viewporting.
- Segment by grapheme clusters.
- Render the real prompt cursor.
- Handle combining marks, emoji/ZWJ sequences, and CJK widths.

### 5.5 Improve responsive layout and non-color cues

- Add clear focus and status symbols/text.
- Define breakpoints for narrow and short terminals.
- Prevent sidebar/prompt/status areas from starving the main pane.
- Add a high-contrast or non-color theme option.

### 5.6 Add clipboard policy and feedback

- Add configurable clipboard modes such as `disabled` and `osc52`.
- Bound OSC52 payload size.
- Report success/failure visibly.
- Keep sensitive clipboard behavior opt-in where appropriate.

### 5.7 Fix backend-specific labels

- Never show Pi-specific fallback text for Claude Code panes.
- Use agent-kind-specific configuration hints and labels.

## Required tests

- Every reserved key in command and passthrough modes.
- Shell/readline controls reach the child in passthrough mode.
- Mouse-enabled children receive click/drag/release/modifier events; Shift-drag remains local.
- Keyboard-only scrolling, selection, and copying.
- Combining marks, emoji, ZWJ, CJK, long prompts, cursor movement, and deletion.
- Narrow terminal snapshots and non-color state/focus assertions.
- Clipboard disabled, oversized, success, and failure paths.

## Completion criteria

- Users can operate both the multiplexer and full-screen terminal applications without irrecoverable key conflicts.
- Core navigation/copy workflows are keyboard accessible.
- Status and focus do not depend on color alone.
- Prompt editing handles Unicode graphemes correctly.

---

# Phase 6 — Compatibility, tests, dependencies, and release hygiene

## Goal

Make compatibility claims enforceable, remove incomplete paths, and reduce supply-chain and release risk.

## Primary files

- `crates/protocol/src/lib.rs`
- `tests/`
- `.github/workflows/ci.yml`
- `.github/dependabot.yml`
- `deny.toml`
- `flake.nix`
- `justfile`
- `extensions/package.json`
- `extensions/package-lock.json`
- `CONTRIBUTING.md`
- `README.md`

## Work packages

### 6.1 Add wire compatibility fixtures

- Check in exact postcard bytes for every client/server message.
- Keep fixtures for each historical supported protocol version.
- Require intentional fixture updates and a protocol version bump for wire changes.
- Add malformed, truncated, oversized, and trailing-byte fixtures.

### 6.2 Add fuzzing and deterministic fault injection

Targets should include:

- protocol framing and decoding;
- state decoding and migration;
- hostile ID normalization;
- terminal response/control parsing;
- selection and Unicode rendering;
- daemon lifecycle state transitions.

Add failpoints or injectable interfaces for spawn, kill, wait, socket write, fsync, rename, and channel failure.

### 6.3 Complete or remove incomplete public paths

Review and either finish, document as experimental, or remove:

- production use of the process-agent `send_prompt` path;
- unused/bypassed paste and scroll protocol variants;
- `PtySpawn.program` behavior;
- command instrumentation that textually appends flags to arbitrary shell expressions.

For Pi/Claude instrumentation, prefer explicit placeholders or environment variables. Legacy shell commands without placeholders should run unchanged with status integration disabled and visibly explained.

### 6.4 Migrate deprecated extension dependencies

- Replace deprecated `@mariozechner/*` Pi packages with supported `@earendil-works/*` packages or a types-only package.
- Regenerate `extensions/package-lock.json`.
- Investigate and resolve the reported npm vulnerabilities without using an unreviewed breaking `npm audit fix --force`.
- Keep lifecycle scripts disabled unless a reviewed dependency requires them.

### 6.5 Tighten dependency policy

- Review cargo-deny duplicate families.
- Either deny duplicates with narrow exceptions or document why each family remains.
- Remove stale unmatched license allowances when appropriate.
- Keep audit and deny behavior consistent across local docs, `just ci`, Nix, and GitHub Actions.

### 6.6 Improve the Nix contributor environment

- Add `cargo-deny` to the development shell.
- Ensure Nix-provided tools are not shadowed by `$HOME/.cargo/bin`.
- Keep sandbox runtime paths deterministic and writable only within the sandbox.
- Expand checks carefully without duplicating the full native PTY lane.

### 6.7 Decide release intent

If releases are intended:

- add reproducible packaging for supported platforms;
- publish checksums and provenance/SBOM artifacts;
- define protocol/client/server release ordering;
- test upgrades from the previous state/protocol version.

If publication is not intended yet:

- set `publish = false` for crates where appropriate;
- document supported installation paths.

## Required tests

- Decode every historical wire fixture and compare current exact encodings.
- Fuzz targets run for a bounded CI smoke duration.
- Shell-semantics tests cover pipelines, `&&`, comments, backgrounding, variables, globbing, and quoting.
- Extension typechecks using the supported Pi package.
- CI policy check rejects mutable action refs, missing `--locked`, and unpinned tool installs.
- Upgrade tests cover previous state and protocol boundaries.

## Completion criteria

- Wire compatibility changes cannot occur silently.
- High-risk parsers and state transitions have fuzz/fault coverage.
- No unexplained incomplete protocol or agent path remains.
- Extension dependencies are supported and reviewed.
- Release or non-release intent is explicit.

---

# Phase 7 — Optional larger architecture

These projects should begin only after Phases 2–4 establish correct lifecycle, identity, and resource limits.

## 7.1 Authoritative daemon/service ownership

Consider moving durable state and PTY lifecycle ownership into one long-lived component so multiple clients become views rather than competing writers.

Questions to settle first:

- Is the daemon authoritative for metadata, or only PTYs?
- How are migrations and crash recovery performed?
- How does an offline client inspect state?
- What is the authentication/ownership model on non-Linux Unix platforms?

## 7.2 Structured screen snapshots

Raw retained byte suffixes may not reconstruct arbitrary fullscreen state after truncation.

- Consider server-side terminal emulation or a versioned screen snapshot.
- Include dimensions, cursor, modes, styles, scrollback metadata, and output watermark.
- Keep raw replay as a compatibility path only if its ordering and bounds are explicit.

## 7.3 Portable peer credentials

Linux uses `SO_PEERCRED`; other Unix platforms currently rely primarily on filesystem permissions.

- Add macOS/BSD peer credential checks where supported.
- Require privately owned immediate socket parents.
- Add platform-specific tests and documented fallback behavior.

## 7.4 Multi-client collaboration

Only consider true simultaneous attachment after leases, ordering, and authoritative state exist.

- Define input arbitration.
- Define resize ownership.
- Distinguish observers from controllers.
- Add auditability for takeover and control transfer.

---

# Suggested new-session prompt

Copy this into a new coding session and replace `PHASE_NUMBER`:

```text
Read AGENTS.md, README.md, docs/DAEMON.md, and docs/REMAINING_WORK.md completely. Inspect git status and the existing Phase 1 diff; do not revert or reimplement it. Implement only Phase PHASE_NUMBER from docs/REMAINING_WORK.md, keeping the change narrow and preserving the documented shell-command semantics. Add the required regression tests and update protocol version/docs when applicable. Run cargo fmt, locked workspace tests, locked workspace clippy with -D warnings, cargo audit, cargo-deny, the Rust 1.88 check, extension typecheck, actionlint, and the Nix check. Report changed paths, design decisions, validation, and any explicitly deferred item.
```

## Recommended next action

Finish the deferred **Phase 3.3 production status-bridge reconnect/finalization test and cleanup**, then the **Phase 3.4 authoritative Pi/Claude transcript event contract and restart test**. Do not proceed on the assumption that raw PTY bytes or process stdout read chunks are structured message boundaries. The other Phase 3 foundations are implemented, but the phase remains partial until these items are complete.
