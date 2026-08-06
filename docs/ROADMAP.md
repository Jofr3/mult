# Roadmap

**This is the entry point for planned work on `mult`.** It replaces the split
that existed until now, where `README.md` and `CONTRIBUTING.md` pointed at
`REMAINING_WORK.md` (a phase-structured handoff plan) while the audit backlog
that work was actually being done from was linked from nowhere. There is one
roadmap; this page is its front door.

## Where work is tracked

| Document | What it is |
| --- | --- |
| [BACKLOG.md](BACKLOG.md) | **The item list.** Every known defect and improvement, with a stable ID (`A2`, `C3`, `H9`, …), a severity, the file and line it was found at, and a status. This is what gets marked `done`. |
| [PLAN.md](PLAN.md) | **The execution order.** Groups backlog items into slices `R1`–`R12`, each with a rationale, a file list, and a "done when". The progress table at the bottom is the current state of the effort. |
| This file | Standing rules, open design decisions that are not yet backlog items, and work deliberately parked beyond the current effort. |

A backlog ID is the right way to refer to work: "fixes `C7`" in a commit message
or PR, `docs/BACKLOG.md` `H9` in an issue.

The backlog is a rebase of an earlier audit onto a substantially rewritten
`main`; its header explains the triage. Items reading **Fixed by `main`** are
closed and must not be re-fixed.

## Standing rules

These are invariants of the codebase, carried forward from the phase plan
because they encode decisions that are easy to undo by accident. The
contributor-facing version is in [../AGENTS.md](../AGENTS.md) and
[../CONTRIBUTING.md](../CONTRIBUTING.md).

1. **Command execution semantics are deliberate and differ per path.**
   `pi_agent_command`, `claude_code_command` and `TerminalLaunch::Command` are
   run through the login shell (`$SHELL -lc`) and are shell-evaluated on
   purpose. `MULT_AGENT_CMD` is split into argv with no shell. Do not unify
   them.
2. **Protocol changes are versioned, and client and daemon change together.**
   A wire change bumps `PROTOCOL_VERSION` in the same commit as both ends.
   Across the current effort every wire change lands in a *single* bump, not one
   per slice — see PLAN.md's ground rules.
3. **Durable state changes are explicit migrations.** Bump `STATE_VERSION`, add
   a migration, keep a golden fixture for every historical version, and preserve
   future-version files byte-for-byte.
4. **Every fixed race or failure path gets a deterministic regression test.**
   No sleeps used as correctness synchronisation.
5. **State files, runtime files and IPC paths stay owner-private.** Ownership
   and mode are verified, not assumed. See [../SECURITY.md](../SECURITY.md).
6. **No new runtime dependencies.** Dev and CI tooling must be named by the
   slice that introduces it.
7. **One concern per pull request.** Do not mix UX changes with daemon lifecycle
   changes.
8. **Every slice ends green.** `just ci`, plus the MSRV check and
   `nix flake check` where they are cheap to run.

## Open decisions carried over

Design questions the owner recorded and has not yet answered. They are not
defects, so they are not backlog items; they gate work that is.

### Authoritative chat message boundaries

The largest one. `mult` persists workspace/chat/terminal metadata and structured
messages from the experimental process-agent backend, but **normal `pi` and
Claude Code chats run in PTYs and have no structured transcript**. Raw PTY output
survives a client reconnect while the same daemon session lives; it does not
survive daemon loss, and it is not searchable history.

`src/transcript.rs` is a bounded, versioned, truncation-recovering append-only
journal built for this, and it is deliberately **unwired** — it has no call site
outside its own tests (`S1`), and it must not be wired until `S2` fixes its two
destructive side effects on a caller-supplied path.

What has to be decided before it can be:

- What constitutes an authoritative user / assistant / tool message boundary for
  a `pi` or Claude Code session. **Lifecycle hooks and arbitrary PTY read chunks
  are not message boundaries** — this is the explicit rule; `mult` does not
  invent boundaries by scraping bytes.
- How ordered search hydrates from the journal.
- Retention and compaction, without silently losing records.

The completion test is: persist normal `pi`/Claude Code chat output, restart
*both* the daemon and the TUI, and verify ordered searchable recovery. Until
that test exists, the README documents this output as daemon-lifetime raw
history only, which is what it is.

### Status-bridge finalisation

The `pi`/Claude Code status bridge has production plumbing and daemon-level
regression coverage, but no end-to-end test across disconnect, reconnect and
finalisation, and no tombstone-recovery cleanup coverage. When a recovered
daemon tombstone is what proves finality, decide whether the status journal is
then obsolete or is explicitly retained for bounded rotation.

### Incomplete public paths — finish, document, or remove

Each of these is reachable-looking code that does not do what its shape
suggests. The decision for each is one of *finish it*, *mark it experimental*,
or *delete it* — not "leave it ambiguous".

- **The process-agent `send_prompt` path.** There is no `dyn AgentBackend`, so
  the call graph is closed and `send_prompt` has no production caller. An
  earlier branch deleted ~700 lines here; that is now the wrong move, because
  `src/transcript.rs` builds on those types. The residual action is
  documentation honesty (`E12`), not deletion.
- **`MULT_AGENT_CMD` and chat search.** Documented as live knobs; the first is
  inert and the second searches an always-empty transcript. Mark, do not remove
  (`E12`).
- **Unused paste and scroll protocol variants**, and `PtySpawn.program` (`F17`).
- **Command instrumentation that textually appends flags to an arbitrary shell
  expression.** Prefer explicit placeholders or environment variables. A legacy
  shell command without a placeholder should run unchanged, with status
  integration disabled and *visibly explained* rather than silently absent.

### Extension dependencies

`extensions/package.json` still depends on the deprecated `@mariozechner/*` Pi
packages, and npm reports four vulnerabilities (one moderate, three high) in
that tree. The intended move is to the supported `@earendil-works/*` packages or
a types-only package, with `package-lock.json` regenerated and lifecycle scripts
kept disabled — resolved by review, not by `npm audit fix --force`.

### Release and publication intent

**Decided, and implemented.** Tag-triggered binary releases for Linux
(gnu + musl) and macOS (x86_64 + aarch64), verified against the crate version
and gated on the test suite. See [RELEASING.md](RELEASING.md), which also lists
what was consciously left out: signing, provenance, SBOM, reproducibility,
crates.io publication, upgrade tests, and distro packaging.

## Longer-horizon work not in the backlog

Preserved from the phase plan. These are real intent, but they are larger than a
backlog item and are not scheduled.

**Parser and resource bounds.** The VTE stack can accumulate an unterminated OSC
sequence independently of the scrollback limit. Either adopt a parser with
explicit sequence limits, or add a strict control-string byte budget bounding
OSC, DCS, APC, PM, SOS and CSI state independently, with normal parsing resuming
after an oversized sequence. The visible scrollback cap is not parser-state
memory safety. (The backlog covers the *panics* — `A13`, `A14` — not this.)

**Command and passthrough modes.** Global shortcuts currently consume common
shell/readline keys. An explicit leader key or a command/passthrough mode would
let ordinary control keys reach the child, with mode and focus shown by
text/symbol rather than colour, a configurable keymap with conflict validation,
and a preserved emergency quit. Prerequisite: the shared binding table from
`E4`/`F13`.

**Full mouse-protocol forwarding.** Wheel forwarding to a mouse-grabbing child
landed; click, release, drag, motion and modifiers have not. Reserve a
documented modifier (Shift) for local selection, and keep local behaviour when
the child has not enabled mouse reporting.

**Keyboard-only scroll, selection and copy.** Line/page scroll, top, bottom,
selection start/extend, copy and clear — exposed through the command palette and
working without mouse capture.

**Grapheme-aware prompt editing.** `E7` adds a cursor and the standard motions.
Beyond it: grapheme-cluster segmentation, horizontal viewporting, and correct
handling of combining marks, emoji/ZWJ sequences and CJK widths.

**Wire compatibility fixtures.** Checked-in exact postcard bytes for every
message, retained per historical protocol version, so a wire change cannot
happen silently without an intentional fixture update. `G2` covers malformed and
truncated framing; it does not cover golden encodings.

**Fault injection.** Injectable failure points for spawn, kill, wait, socket
write, fsync, rename and channel send. `G3` brings back fuzzing; this is the
other half.

**Nix contributor environment.** The dev shell's `shellHook` prepends
`$HOME/.cargo/bin` to `PATH`, so a rustup-installed tool shadows the one the
flake provides. Decide which should win — the flake's pinned Rust or the
`rust-toolchain.toml` pin via rustup shims — and make the shell say so.

**Authoritative daemon ownership.** Moving durable state as well as PTY
lifecycle into the daemon, making clients views rather than competing writers.
Settle first: is the daemon authoritative for metadata or only PTYs; how do
migrations and crash recovery work; how does an offline client inspect state;
what is the ownership model on non-Linux Unix platforms.

**Structured screen snapshots.** A retained raw byte suffix cannot always
reconstruct arbitrary fullscreen state after truncation. A versioned snapshot
carrying dimensions, cursor, modes, styles, scrollback metadata and an output
watermark would; raw replay would remain a compatibility path with explicit
ordering and bounds.

**Portable peer credentials.** Linux uses `SO_PEERCRED`. `C3`/`F7` make the
check fail closed everywhere from one implementation; beyond that, add real
macOS/BSD credential checks, require privately owned immediate socket parents,
and test per platform.

**Multi-client collaboration.** Only after leases, ordering and authoritative
state exist. Needs input arbitration, resize ownership, an observer/controller
distinction, and auditability for takeover and control transfer.

## Historical documents

Kept for provenance. **Do not read these as current** — they describe states of
the project that no longer hold, and several of their proposed fixes are now
wrong.

| Document | Why it is here |
| --- | --- |
| [REMAINING_WORK.md](REMAINING_WORK.md) | The Phase 1–7 handoff plan. Its Phase 1–3 status claims are stale, and it was the second roadmap this file exists to retire. Everything in it that is still live has been carried into the backlog or into the sections above; it is retained as the record of the original phase design and its completion checklists. |
| [BACKLOG-v1.md](BACKLOG-v1.md) | The first audit, run against an older commit. Superseded by BACKLOG.md, which re-triaged every item against the current code. |
| [PLAN-v1.md](PLAN-v1.md) | The execution plan for that first audit. Superseded by PLAN.md. |

Operational documentation that *is* current lives in
[DAEMON.md](DAEMON.md), [CONFIG.md](CONFIG.md),
[TROUBLESHOOTING.md](TROUBLESHOOTING.md) and [RELEASING.md](RELEASING.md).
