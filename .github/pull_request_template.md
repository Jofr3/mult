<!--
Keep changes narrow and buildable. See CONTRIBUTING.md and AGENTS.md.
-->

## What this changes

<!-- One or two sentences. If it fixes a backlog item, name it (e.g. "Fixes B7"). -->

## Why

<!-- The behaviour that was wrong, or the need this meets. -->

## Checklist

- [ ] `just ci` passes (`fmt-check`, `version-check`, `lint`, `test`, `deny`, `typecheck`)
- [ ] New behaviour has a test, or there is a note below saying why one is not feasible
- [ ] `CHANGELOG.md` has an entry under `[Unreleased]`
- [ ] Docs updated if user-visible behaviour, config keys, CLI flags or keybindings changed
- [ ] No new runtime dependency — or it is called out and justified below
- [ ] `Cargo.lock` is in sync if dependencies changed

## Notes for the reviewer

<!--
Anything worth knowing: a deliberate behaviour change, a tradeoff, something you
were unsure about, or a path you could not test. If this touches `state.json`,
`config.json`, the daemon socket or process spawning, say what you checked —
those are security-sensitive (SECURITY.md).
-->
