# mult — improvement plan (v2, rebased)

Execution plan for [BACKLOG.md](BACKLOG.md), rebased onto `main` at `2a91632`.
Start at [ROADMAP.md](ROADMAP.md) — it is the entry point for this plan, the
backlog, the standing rules, and the open decisions that are not yet items.
Superseded v1 artifacts: [BACKLOG-v1.md](BACKLOG-v1.md), [PLAN-v1.md](PLAN-v1.md).

## Ground rules

1. **Every slice ends green:** `cargo fmt --all`, `cargo clippy --workspace --all-targets
   --all-features -- -D warnings`, `cargo test --workspace --all-targets --all-features`,
   `cargo test --test pty_integration`, and `nix flake check` where it is cheap to run.
2. **No new runtime dependencies.** Dev/tooling deps must be named by the slice.
3. **One wire bump for the whole effort.** `main`'s `PROTOCOL_VERSION: u16 = 10` is the
   baseline. Every protocol change across all slices lands as a single move to `11`; slices
   that need one coordinate rather than each bumping.
4. **Do not re-fix what `main` already fixed** — the backlog lists those explicitly per section.
5. **Do not port a v1 fix blind.** v1 lives at `hardening/audit-remediation` (`f451667`) and
   is reference material; several of its remedies are now wrong (F1, C1, A3/C12, F11).
6. Every item gets a regression test where feasible; update `BACKLOG.md` status and append to
   `CHANGELOG.md` as part of the slice.

## Why this order

`main`'s test suite is trustworthy (v1's G1 was independently fixed here and verified by
sabotage), so correctness can come first without a harness slice. The daemon's global-lock
discipline leads because it is the only *critical* item and everything else in the daemon
interacts with it. Architecture is late again, and now *smaller* — `main` restructured
differently, so several v1 refactors are reduced in scope or dropped.

---

### R1 — Daemon lock discipline and pane lifecycle
**Items:** A2, N1, N2, N3, A1, A8, A11, A12, A9
**Files:** `src/bin/mult-server.rs`

The only critical cluster. `main` routes input/resize/attach through the global `ServerState`
mutex and holds it across the pane operation, so a blocking PTY write or a 32 MiB attach
replay freezes every pane and every client. `pane_by_id`'s dead linear scan takes every pane
mutex under that same lock, multiplying it.

**Done when:** no blocking I/O or replay happens under the server lock; a per-pane writer
thread with a bounded queue absorbs PTY writes and reports refusal via `LeaseRejected`;
history trimming is O(bytes dropped); shutdown has a deadline and always unlinks the socket;
replay cannot evict the client it just attached and cannot pin ~64 MiB per attach.

---

### R2 — Client responsiveness
**Items:** N4, B6, B5, B7, B11
**Files:** `src/pty.rs`, `src/runtime.rs`

A dead daemon with N terminals currently freezes the UI for ~2N seconds *every retry frame*.
**Constraint:** `main` rebuilt the client as synchronous request/response with idempotency
keys and request caching — only connection establishment can move off-thread; the correlated
waits cannot without a redesign. Do not import v1's design wholesale.

**Done when:** reconnect does not serially block on N × 2 s; connection setup is off the render
thread; sockets are `shutdown` before being dropped; `drain_events` has a per-frame budget;
draw/input errors no longer kill the session without cleanup.

---

### R3 — Idle cost and save discipline
**Items:** D1, S3, S4, B3, B9, B16, D5, S5
**Files:** `src/runtime.rs`, `src/app.rs`, `src/storage.rs`, `src/git.rs`

Idle is noisier than at v1 time: two unconditional `Resize` sites (125 writes/s), a status
bridge doing ~1250 syscalls/s with 4 chats, a `git` fork per workspace every 2 s, and `main`'s
own redraw gating defeated twice a minute by a discarded return value.

**Done when:** resize fires only on change; status polling is timed; the git probe neither
forks nor blocks the UI thread; saves are rate-limited and go through the *locked* store only;
no blocking `flock` on the render thread.

---

### R4 — Security
**Items:** C3, F7, C2, C6, S2, C7, C8, C9, C13, C14, S6, S7, S8
**Files:** `src/pty.rs`, `src/bin/mult-server.rs`, `new crates/protocol/src/peer.rs`,
`src/config.rs`, `src/storage.rs`, `src/git.rs`, `src/transcript.rs`, `src/runtime.rs`,
`extensions/`, `SECURITY.md`, `docs/DAEMON.md`

Peer verification still fails **open** off Linux while the docs imply otherwise; config is the
one file `main` did not harden, and it feeds `$SHELL -lc` auto-started commands. F7 lands
first so C3 fixes one implementation, not three.

**Done when:** peer verification fails closed on every platform from a single shared
implementation; config reads are `O_NOFOLLOW`/owner/mode/size-checked like state reads; the
state read is size-capped; `TranscriptJournal::open` no longer truncates files or `fchmod`s
directories as a side effect; the git probe cannot load a hostile repo's config; the daemon
binary is ownership-checked and spawned with an allow-listed environment; docs claim only what
is enforced.

---

### R5 — Emulator panics and fuzzing
**Items:** A13, A14, G3, G4, D6, D7
**Files:** `src/pty.rs`, `crates/protocol/src/lib.rs`, `src/runtime.rs`, `src/ui.rs`, new `fuzz/`

A13 (1-row/1-column panes panic the parser on a stray byte) was found by v1's fuzz target on
its first run, so the targets come back — and this time A14 is in scope too. G4 must land
before D7, since D7 changes exactly the code whose "behaviourally identical" claim is untested.
**Note:** an untracked `fuzz/` of build residue is currently polluting `git status`; clean it.

**Done when:** no pane dimension can reach the parser below its safe floor, and a pane too
small to draw degrades visibly instead of panicking; A14 has a decided outcome (fix or
documented workaround); both fuzz targets build and run clean; batched-vs-per-byte equivalence
is tested; the CSI path does not allocate per sequence.

**Authorised tooling:** `cargo-fuzz` in a separate non-workspace `fuzz/`.

---

### R6 — Render performance
**Items:** D2, D3, D8, D9, D10, D11, G6, G12
**Files:** `src/ui.rs`, `src/config.rs`, `src/pty.rs`

v1 measured this path at 1.105 ms and ~30 000 allocations per frame, and the same shapes are
all present. Snapshots land here to pin the refactor.

**Done when:** the per-cell double allocation is gone; the search scrape is lazy; the palette
is parsed once and single-sourced; full-buffer snapshot coverage exists including a narrow
case; the vt100 adapter is directly tested.

**Authorised tooling:** `insta` (dev-dependency).

---

### R7 — CLI, error surfacing and config validation
**Items:** E1, E2, E5, E6, E11, E12, E9, E10, G13, G14
**Files:** `src/main.rs`, `src/bin/mult-server.rs`, new `src/cli.rs`, `src/config.rs`,
`src/storage.rs`, `src/runtime.rs`, `src/ui.rs`, `src/git.rs`

E2's status surface unblocks B8. E12 is F1's residual: `MULT_AGENT_CMD` and chat search are
documented as working while being inert — mark them, do not remove them.

**Done when:** both binaries have real CLIs; config errors name file and position; unknown keys
and bad colours are reported; a decode that must reset tells the user where the backup went;
`NO_COLOR` is honoured; config can be reloaded.

---

### R8 — Interaction affordances
**Items:** E4, E7, E8, F13, F21
**Files:** `src/app.rs`, `src/ui.rs`, `src/runtime.rs`

**Done when:** a help overlay and the command palette generate from one binding table; prompts
have a cursor with the standard motions; status is carried by shape as well as hue; the four
duplicated list/prompt handlers are one.

---

### R9 — Architecture, mechanical
**Items:** F2, F3, F12, F14, F17, F18, F19, F20
**Files:** `src/pty.rs`, `src/model.rs`, `src/app.rs`, `src/ui.rs`, `src/storage.rs`,
`crates/protocol/`

Strictly behaviour-preserving. Scope changed from v1: F2 is *larger* (12 parallel maps, not
8), F3's premise inverted (`Default` is now the production constructor), F19 is *reduced* to
deleting the three alias pairs, and F17 must keep `terminal_all_lines`, which is now live.

---

### R10 — Architecture, structural
**Items:** F9, F15, F5, F6, F8, F10, F16
**Files:** `src/runtime.rs` → `src/runtime/`, `src/ui.rs` → `src/ui/`, `src/app.rs` →
`src/app/`, new `src/layout.rs`, `src/lib.rs`, `crates/protocol/`

`main` did not do v1's module split, so every v1 fix phrased against `src/runtime/*` or
`src/app/*` needs its path rewritten before porting. F16 is materially bigger than v1: it must
now coexist with `STATE_VERSION = 2`, its V1→V2 migration, and the future-version
byte-preservation guarantee. F8 carries the single wire bump if one is still outstanding.

---

### R11 — Tests
**Items:** G2, G5, G7, G9, G10, G11, G12*, S1, S9, S10
**Files:** `crates/protocol/`, `src/storage.rs`, `src/config.rs`, `src/agent.rs`,
`src/bin/mult-server.rs`, `tests/`, `flake.nix`

*G12 lands in R6 with the code it covers.

**Done when:** framing has chunked-read and malformed-input coverage; storage shape errors and
backup failures are tested; no test mutates process-global env; chunk boundaries are tested at
`CHUNK±1`; CI asserts the PTY integration tests actually *ran* (S9 — the unmet half of v1's
G1); the flake's shell env matches what the code reads (S10).

---

### R12 — CI, docs and release
**Items:** S11, S12, H3, H4, H5, H7, H8, H9, H10, H11, H12, H13, H14, H15, H16, H17
**Files:** `.github/`, `justfile`, `flake.nix`, `README.md`, `CONTRIBUTING.md`, `CHANGELOG.md`,
`docs/`, `.gitignore`, `Cargo.toml`

S12 is the important one: `README.md` and `CONTRIBUTING.md` point at `docs/REMAINING_WORK.md`
as the authoritative follow-up list, but it is a stale Phase-3 handoff plan and this backlog is
linked from nowhere. **Reconcile them into one roadmap** rather than leaving two.

**Done when:** the MSRV job provably tests 1.88 (`+1.88`); `just ci` matches CI; coverage is
measured; advisories run on a schedule with the audit/deny redundancy resolved; the config
reference, troubleshooting guide and layout table are accurate; a release workflow verifies
tag↔version and runs tests before publishing; the docs tree has one roadmap.

---

## Progress

| Slice | Title | Status |
|-------|-------|--------|
| R1 | Daemon lock discipline and pane lifecycle | done |
| R2 | Client responsiveness | done |
| R3 | Idle cost and save discipline | done |
| R4 | Security | done |
| R5 | Emulator panics and fuzzing | done |
| R6 | Render performance | done |
| R7 | CLI, error surfacing and config validation | todo |
| R8 | Interaction affordances | todo |
| R9 | Architecture, mechanical | todo |
| R10 | Architecture, structural | todo |
| R11 | Tests | done |
| R12 | CI, docs and release | done |
