# mult — improvement plan (v1, HISTORICAL)

> **Superseded. Do not read this as current work.** The live execution plan is
> [PLAN.md](PLAN.md); the roadmap front door is [ROADMAP.md](ROADMAP.md).
> Retained for provenance only.

Execution plan for [BACKLOG-v1.md](BACKLOG-v1.md). Work proceeds in vertical slices, one at a
time, each landing as an independently reviewable, buildable change.

## Ground rules

1. **Every slice ends green.** `cargo fmt --all`, then
   `cargo clippy --workspace --all-targets --all-features -- -D warnings`, then
   `cargo test --workspace --all-targets --all-features`. A slice is not done until all
   three pass.
2. **No new dependencies** unless the slice explicitly authorises one (per `AGENTS.md`).
   Slices that need one say so and name it.
3. **Behaviour-preserving unless the item says otherwise.** Refactor slices must not change
   observable behaviour; fix slices change exactly what the backlog item describes.
4. **Each backlog item gets a regression test** where one is feasible, in the same slice.
5. **Update `BACKLOG.md` status** (`todo` → `done`, or `dropped` with a reason) as part of
   the slice. Add a `CHANGELOG.md` entry under `[Unreleased]`.
6. **Verify before deleting.** Items marked "verify first" (F1) require evidence before any
   removal.

## Slice order and rationale

Test trustworthiness comes first — the integration suite currently passes vacuously (G1),
so nothing after it can be verified until that is fixed. Then correctness by blast radius
(daemon → client → loop), then security, then performance, then UX, then the two
architecture passes, then docs and release. Architecture is deliberately late: the
structural refactors are safest once the test suite is real and the bugs are gone.

---

### Slice 1 — Make the test harness trustworthy
**Items:** G1, G7, G11, H2, H3, H4
**Files:** `tests/pty_integration.rs`, `crates/protocol/src/lib.rs`, `src/config.rs`,
`src/storage.rs`, `justfile`, `flake.nix`, `.github/workflows/ci.yml`

The integration suite returns `None` → green whenever setup fails, so the entire
reconnect/attach/exit safety net may already be dead on CI. Fix that first, remove the
env-mutating race, de-flake the wall-clock deadlines, add `--locked`, and make `just ci`
actually match CI.

**Done when:** integration tests fail loudly if the server does not come up; no test mutates
process-global env; `just ci` runs deny + typecheck; `cargo-deny` is in the dev shell.

---

### Slice 2 — Daemon hot path and pane lifecycle
**Items:** A1, A4, A5, A6, A7, A8, A9, A12, G8, G9
**Files:** `src/bin/mult-server.rs`, `crates/protocol/src/lib.rs`

The daemon's throughput ceiling is ~3.6 MB/s per pane because of an O(32 MiB) memmove per
read held under the pane mutex, and a single failing pane operation disconnects every pane
on the connection. Both are user-visible today.

**Done when:** history trimming is O(trimmed); per-pane failures no longer tear down the
connection; `Stop` kills the process group and the reader thread reaches EOF; evicted
clients are notified; attach does not clone under the lock or evict the client it just
attached; dispatch-loop and chunk-boundary tests exist.

---

### Slice 3 — Client PTY integrity
**Items:** B1, B5, B7, B8, B12, B15, C9, D6, D7, D10, D11, G2, G4
**Files:** `src/pty.rs`, `crates/protocol/src/lib.rs`, `src/runtime.rs` (call sites only)

Unmapped panes currently materialise parsers that are never reclaimed, reconnects leak
threads and fds, and the frame drain has no work budget. Also lands the escape-sequence
batching and the CSI allocation removal, since they are the same code region.

**Done when:** unknown pane ids are dropped, not synthesised; reconnect shuts the old socket
down; `drain_events` has a per-frame budget; `ServerMessage::Error` carries a pane; terminal
query responses are capped per chunk; property tests cover framing and batched-vs-per-byte
parser equivalence.

---

### Slice 4 — Event loop, persistence and agent robustness
**Items:** B2, B3, B4, B9, B10, B11, B13, B14, D1, D4, D5, G5, G10
**Files:** `src/runtime.rs`, `src/storage.rs`, `src/model.rs`, `src/agent.rs`, `src/git.rs`

Idle cost is the theme: a `Resize` on the wire every 16 ms, a `mkdir` + ancestor `lstat`
walk per chat per tick, a `git` fork per workspace every 2 s on the UI thread, and two
`fsync`s per frame while an agent streams. Plus the stale-status-file bug that pins a
finished chat at "Thinking" forever, and the save/draw errors that kill the session.

**Done when:** resize is sent only on change; status polling is cached and timed; saves are
rate-limited and non-fatal; the git probe is off the UI thread; the agent event channel
cannot deadlock; storage shape/version errors are tested.

**Authorised dependency:** none — use `std::thread` + `mpsc` for the git probe.

---

### Slice 5 — Security hardening
**Items:** C2, C3, C4, C5, C6, C7, C8, C10, C11, C13, C14, F7
**Files:** `src/config.rs`, `src/storage.rs`, `src/pty.rs`, `src/bin/mult-server.rs`,
`src/git.rs`, `src/runtime.rs`, `crates/protocol/src/` (new `peer.rs`),
`extensions/mult-status.ts`, `extensions/mult-claude-status.sh`, `SECURITY.md`,
`docs/DAEMON.md`

The peer-credential check silently passes on macOS/BSD while the docs claim it is universal;
config is read symlink-following with no owner check and then shell-evaluated and
auto-started; the status path falls back to the very directory the privacy check rejected.
F7 (dedup the peer check into `crates/protocol/src/peer.rs`) lands here because C3 must fix
one implementation, not two.

**Done when:** the peer check is implemented for BSD/macOS and an unavailable check is a hard
failure; config and state reads are `O_NOFOLLOW`, regular-file-checked and size-capped; the
status path fails closed; the git invocation cannot load a hostile repo's config; the daemon
binary is ownership-checked and spawned with a minimal environment; generated runtime files
are cleaned up; extension temp files use `O_EXCL` and private modes; `SECURITY.md` and
`docs/DAEMON.md` match reality.

---

### Slice 6 — Render performance and theme
**Items:** D2, D3, D8, D9, F20 (palette half), G6, G12
**Files:** `src/ui.rs`, `src/config.rs`

Measured: 414 µs and 20 000 allocations per frame from the vt100 screen copy alone, plus a
full screen scrape every frame for a search that is not active, plus 12 hex parses per
frame. Snapshot tests land here so the refactor is pinned.

**Done when:** the redundant `to_string()` is gone and the symbol is stored without a second
allocation; search line scraping is lazy; the palette is parsed once at config load and
derived from a single source; `insta` snapshots cover the default frame, the palette, and a
narrow 80×24 layout.

**Authorised dependency:** `insta` (dev-dependency only).

---

### Slice 7 — CLI, error surfacing and config validation
**Items:** E1, E2, E5, E6, E9, E10, E11, G13 (G14 landed early in Slice 5, which
rewrote `git::current_branch`; E11 moved in from the storage group, E8 moved out
to Slice 8 — it is an accessibility change to status glyphs and belongs with the
other interaction work)
**Files:** new `src/cli.rs`; `src/main.rs`, `src/bin/mult-server.rs`,
`src/config.rs`, `src/storage.rs`, `src/model.rs`, `src/app.rs`, `src/runtime.rs`,
`src/ui.rs`

`mult --version` currently launches the TUI; a bad config dies with a `Debug` dump that does
not name the file; a failed daemon connection writes its explanation into a pane that cannot
exist. All three are first-contact failures.

**Done when:** `--help`/`--version`/`--config`/`--state`/`--socket` work on both binaries;
config errors name the file and position; unknown config keys are rejected and bad colours
are reported; a global status line surfaces runtime errors; `state.json` decoding is lenient
and an unavoidable reset names its backup; `NO_COLOR` is honoured; config can be reloaded
from the palette.

**Authorised dependency:** none — hand-rolled argv parsing (`clap` is out per `AGENTS.md`).

---

### Slice 8 — Interaction affordances
**Items:** E3, E4, E7, E8 (moved from Slice 7)
**Files:** `src/app.rs`, `src/runtime.rs`, `src/ui.rs`

Destructive delete is one key away from three constructive bindings and takes the parent
workspace with it; there is no in-app help at all; prompt input is append-only.

**Done when:** deleting a chat/terminal with content asks first; a `?`/`F1` overlay lists
bindings from the same table the command palette uses; prompts support a cursor with
Left/Right/Home/End and Ctrl+w/u/a/e; sidebar status glyphs differ by shape, not only hue.

---

### Slice 9 — Concurrency and isolation (design-heavy)
**Items:** A2, A3, A10, B6, C1, C12
**Files:** `src/pty.rs`, `src/bin/mult-server.rs`, `crates/protocol/src/lib.rs`,
`src/runtime.rs`, `AGENTS.md`, `SECURITY.md`

The remaining criticals need design, not patches: a two-sided blocking-write deadlock, and
globally-shared session ids that let a second `mult` instance take over the first one's
shells. Also the `state.json` execution boundary.

**Done when:** PTY writes cannot block the socket reader; session ids are namespaced per
client instance so two instances cannot collide; the daemon caps connections and sessions
and keeps an idle deadline; connection establishment is off the render thread; replaying a
persisted `Command` terminal is confirmed or explicitly re-armed, and the boundary is
documented.

---

### Slice 10 — Architecture, mechanical pass
**Items:** F2, F3, F4, F12, F13, F14, F17, F18, F19, F20 (remainder)
**Files:** `src/pty.rs`, `src/model.rs`, `src/app.rs`, `src/ui.rs`, `src/runtime.rs`,
`src/storage.rs`, `crates/protocol/src/lib.rs`

Strictly behaviour-preserving: collapse `PtyRuntime`'s eight parallel maps into one
`PtyPane`, remove the `Default` impl that forks a daemon, make `PtyKey`'s wire invariant
unforgeable, drop the demo seed from `ProjectState::default`, delete dead surface, dedupe the
repeated prompt/list handlers, and finish the naming cleanup.

**Done when:** every listed item is applied, no observable behaviour changed, and
`PROTOCOL_VERSION` is bumped if wire enums were trimmed.

---

### Slice 11 — Architecture, structural pass

Split in two. The semantic changes (the F1 decision, typed errors, the testability seams)
are independent of the module splits and are far riskier to review buried inside a
file-move diff, so they land first and separately.

#### Slice 11a — Semantics: the F1 decision, typed errors, seams
**Items:** F1, F8, F10, F11, F16
**Files:** `src/agent.rs` (deleted), `src/app.rs`, `src/model.rs`, `src/storage.rs`,
`src/runtime.rs`, `src/pty.rs`, `src/ui.rs`, `src/bin/mult-server.rs`,
`crates/protocol/src/lib.rs`, `docs/DAEMON.md`

**F1 was gated** on proving whether the `agent.rs` transcript path is reachable in
production. It is not: see the evidence recorded in the F1 backlog row. The path is
deleted, with a migration that copies the pre-migration `state.json` aside and names the
copy, so a user who somehow has a populated `messages` does not lose it.

No module is split here — `runtime.rs`, `app.rs` and `ui.rs` stay single files.

**Done when:** the F1 decision is recorded with evidence and acted on; a `RejectCode`
travels on the wire and no control flow anywhere matches on error text; `PtyError` and
`StateError` exist with hand-written `Display`/`Error` impls and `io::Result` survives only
at the I/O boundary; `StateStore` and `AgentStatusSource` seams exist with in-memory
doubles; persisted terminal state records intent (`restore_on_launch`) rather than liveness
and the chat seen-bit lives in `ChatStatus::Done`. Migrations are round-trip tested and the
`insta` snapshots are byte-identical.

#### Slice 11b — Structure: the module splits
**Items:** F5, F6, F9, F15
**Files:** new `src/layout.rs`, `src/runtime/`, `src/app/`, `src/ui/`; `src/lib.rs`,
`src/main.rs`

Split the three god-modules and extract layout out of the renderer. Purely structural, on
top of 11a's semantics.

**Done when:** `runtime`, `app` and `ui` are modules under 800 lines each, `runtime` is
reachable from the library so tests stop duplicating fixtures, and `AppLayout::compute` is
called once per iteration and passed to both `ui::draw` and the mouse/resize handlers.

---

### Slice 12 — Docs, CI and release
**Items:** G3, H1, H5, H6, H7, H8, H9, H10, H11, H12, H13, H14, H15, H16, H17
**Files:** `README.md`, `CHANGELOG.md`, `CONTRIBUTING.md`, `docs/`, `.github/`, `justfile`,
`.gitignore`, `flake.nix`

**Done when:** MSRV is either tested or dropped; a tag-triggered release workflow ships
Linux and macOS archives; coverage is measured; advisories run weekly; the config reference,
troubleshooting guide and layout table are accurate; issue/PR templates exist; the CHANGELOG
renders and `v0.1.0` is tagged; fuzz targets exist.

**Authorised dependency:** `cargo-fuzz` targets in a separate `fuzz/` workspace member
(not a dependency of the main crates); `cargo-llvm-cov` as a CI tool.

**Outcome.** All fourteen remaining items landed (H13 was already done). Notes:

- The MSRV was *tested*, not dropped: 1.88 still builds the whole workspace, so the
  declared `rust-version` stands and a pinned CI job now proves it.
- Coverage measured at **87.50% lines**; the CI floor is 85 as a regression guard.
- `cargo audit` was removed in favour of `cargo deny`, resolving the duplication
  `deny.toml` had already flagged.
- The version pins went from four to three (`crates/protocol` now inherits the workspace
  version), and `just version-check` guards the rest inside `just ci`.
- **`v0.1.0` was deliberately not tagged.** The CHANGELOG, the release workflow and the
  checklist are all in place; cutting the tag is the owner's call, and the two compare
  links resolve as soon as it is pushed.
- The `vt_response_detector` fuzz target found a real, previously unknown panic on its
  first run — a 1-row or 1-column PTY pane overflows inside `fnug-vt100`. It is out of
  this slice's scope and is filed as **A13**.

The `fuzz/` directory is a separate workspace, so `cargo build --workspace`, `Cargo.lock`
and `cargo deny` are all unaffected by it.

---

### Slice 13 — Final cleanup
**Items:** A13, G15, F21
**Files:** `src/pty.rs`, `src/runtime/session.rs`, `src/runtime/agent_launch.rs`,
`src/runtime/agent_status.rs`, `src/runtime/mod.rs`, `src/ui/main_pane.rs`,
`src/app/text_input.rs`, `src/bin/mult-server.rs`, `crates/protocol/src/lib.rs`,
`fuzz/fuzz_targets/vt_response_detector.rs`, `flake.nix`, plus the doc pass.

The three items left open, two of which the earlier slices' own tooling found: a real
panic (A13, from the new fuzz target), a red `nix flake check` (G15), and a latent
off-by-a-wrap in the shared list step (F21). Then a consistency pass over every document
against the code the thirteen slices left behind.

**Done when:** no PTY size below 2×2 can reach the emulator, from either end of the wire,
and a pane too small to draw says so; `nix flake check` passes, with no test skipped for any
reason other than the sandbox genuinely not having the operating-system facility it needs;
`ListSelection::step` is a modular wrap for any delta; every backlog row is closed or
carried forward with a reason; and the docs match the code.

**Outcome.** All three landed, and the work turned up two things the audit had not:

- **A14** — a second `fnug-vt100` panic, on a *column shrink that truncates a wide
  character*, at ordinary sizes (80 → 41 columns with CJK on screen). A13's clamp does not
  cover it and no cheap workaround is correct, so it is filed and carried forward with the
  three candidate fixes written down. This is the one open row in the backlog.
- **G15 was three problems, not one.** The six named tests were real, but behind them
  `ensure_private_dir` could not succeed at all in the sandbox (`/` maps to `nobody`, so the
  ancestor walk rejected every path on the system) and nine `mult-server` dispatch tests
  were failing in `openpty`, because the sandbox has a `/dev/ptmx` symlink with no `devpts`
  behind it. The walk now stops at the filesystem root; the nine, which spawn real panes and
  are not fakeable, honour the same explicit `MULT_SKIP_PTY_INTEGRATION` opt-out the
  integration suite already used — set by `flake.nix` and by nothing else, so they run
  everywhere a PTY can actually be allocated.

---

## Progress

| Slice | Title | Status |
|-------|-------|--------|
| 1 | Make the test harness trustworthy | done |
| 2 | Daemon hot path and pane lifecycle | done |
| 3 | Client PTY integrity | done |
| 4 | Event loop, persistence and agent robustness | done |
| 5 | Security hardening | done |
| 6 | Render performance and theme | done |
| 7 | CLI, error surfacing and config validation | done |
| 8 | Interaction affordances | done |
| 9 | Concurrency and isolation | done |
| 10 | Architecture, mechanical pass | done |
| 11 | Architecture, structural pass (11a + 11b) | done |
| 11a | Architecture, semantics (F1, typed errors, seams) | done |
| 11b | Architecture, module splits | done |
| 12 | Docs, CI and release | done |
| 13 | Final cleanup (A13, G15, F21) and the doc consistency pass | done |
