<!--
Read CONTRIBUTING.md and AGENTS.md before opening a PR.
Planned work and its status live in docs/ROADMAP.md.
-->

## What this changes

<!-- One paragraph. What behaviour is different afterwards, and why. -->

## Why

<!-- The bug, backlog item (e.g. "docs/BACKLOG.md H9"), or user-visible need. -->

## Validation

- [ ] `just ci` passes locally (version-check, `cargo fmt --check`, `clippy -D warnings`, tests, `cargo deny`, extension typecheck)
- [ ] New behaviour has a regression test, or the PR explains why one is not feasible
- [ ] `Cargo.lock` is in sync if dependencies changed

<!-- Paste anything notable: a failing test that now passes, a measurement, a snapshot diff. -->

## Risk and scope

- [ ] No new runtime dependency (see `AGENTS.md`)
- [ ] No wire-protocol change, **or** `PROTOCOL_VERSION` is bumped and client and daemon change together
- [ ] No durable state-format change, **or** `STATE_VERSION` is bumped with an explicit migration and fixtures
- [ ] Security-sensitive paths (state files, runtime files, IPC, spawned commands) are unchanged, or the change is described above and in `SECURITY.md` terms

## Docs

- [ ] `README.md` / `docs/` updated if user-visible behaviour, config keys, or env vars changed
- [ ] `CHANGELOG.md` `[Unreleased]` updated
- [ ] `docs/BACKLOG.md` status updated if this closes a tracked item
