# Releasing

`mult` has not cut a release yet. `[0.1.0]` in [../CHANGELOG.md](../CHANGELOG.md)
describes the initial prototype but no `v0.1.0` tag exists, and there are no
published binaries. This document describes the process that is now wired up, so
that cutting the first tag is a decision rather than a project.

## What ships

One archive per platform, `mult-<version>-<target>.tar.gz`, each containing:

- `mult` — the TUI client
- `mult-server` — the PTY daemon
- `LICENSE-MIT`, `LICENSE-APACHE`, `README.md`, `CHANGELOG.md`

plus a `.sha256` sidecar.

**Both binaries are in every archive, and that is not negotiable.** The client
locates the daemon by taking its own `current_exe()` and swapping the file name
for `mult-server`; it does not search `$PATH`. A `mult` installed without an
adjacent `mult-server` cannot autospawn the daemon, and the user has to start it
by hand. The client also checks that the adjacent binary — and its directory —
is a regular file owned by the user or root and not group/other-writable before
executing it, so installing both into one private directory is the supported
path. Upgrading only one of the two also produces the protocol-version mismatch
documented in
[TROUBLESHOOTING.md](TROUBLESHOOTING.md#everything-stopped-working-right-after-an-upgrade).

Targets built by `.github/workflows/release.yml`:

| Target | Runner | Notes |
| --- | --- | --- |
| `x86_64-unknown-linux-gnu` | `ubuntu-latest` | Needs a glibc at least as new as the runner's. |
| `x86_64-unknown-linux-musl` | `ubuntu-latest` | Static; the portable Linux build. |
| `aarch64-apple-darwin` | `macos-latest` | Native (Apple silicon). |
| `x86_64-apple-darwin` | `macos-latest` | Cross-compiled; built but not test-run on that architecture. |

Not shipped: Windows (the code is Unix-socket and Unix-PTY based), and nothing
is published to crates.io.

## The version lives in three places

`crates/protocol` inherits `[workspace.package] version`, so a version bump
touches:

1. `Cargo.toml` — `[workspace.package] version` (the source of truth)
2. `flake.nix` — `packages.default.version`
3. `extensions/package.json` — `"version"`

`just version-check` asserts all three agree (and that the protocol crate still
inherits rather than re-declaring). It runs as the first step of `just ci`, so
a mismatch fails locally before it can reach a tag. Passing an argument also
pins the value: `just version-check 0.2.0`.

## Cutting a release

1. **Decide the version.** Semantic versioning; while `0.x`, breaking changes
   bump the minor.

2. **Bump the three files**, then confirm:

   ```sh
   just version-check 0.2.0
   ```

3. **Close the changelog section.** Rename `## [Unreleased]` to
   `## [0.2.0] - YYYY-MM-DD`, add a fresh empty `## [Unreleased]` above it, and
   update the reference links at the bottom of the file:

   ```markdown
   [Unreleased]: https://github.com/Jofr3/mult/compare/v0.2.0...HEAD
   [0.2.0]: https://github.com/Jofr3/mult/compare/v0.1.0...v0.2.0
   ```

4. **Call out compatibility explicitly** in the changelog if either version
   moved. Both are hard breaks for users:

   ```sh
   grep -n 'PROTOCOL_VERSION: u16' crates/protocol/src/lib.rs
   grep -n 'STATE_VERSION' src/storage.rs
   ```

   A `PROTOCOL_VERSION` bump means every running `mult-server` must be
   restarted. A `STATE_VERSION` bump means the release is a one-way door for
   state files: a newer state file is refused, unmodified, by an older client.

5. **Run the full gate**, plus the things CI runs in separate jobs:

   ```sh
   just ci
   cargo +1.88 check --workspace --locked --all-targets --all-features
   just coverage
   nix flake check
   ```

6. **Merge to `main`.** Tags are cut from `main`, never from a branch.

7. **Tag and push.**

   ```sh
   git tag -a v0.2.0 -m "v0.2.0"
   git push origin v0.2.0
   ```

8. **Watch the workflow.** It runs in four stages and publishes nothing until
   the first two pass:

   - **verify** — `just version-check "${tag#v}"` proves the tag names the
     version the workspace declares, then `just ci` runs the whole gate on the
     tagged commit. A tag that disagrees with `Cargo.toml`, or a commit that
     does not pass tests, stops here with nothing published.
   - **draft** — creates a *draft* GitHub release.
   - **build** — the four targets build and upload their archive and checksum
     to the draft.
   - **publish** — undrafts the release once every target succeeded.

   Because the release is a draft until the last job, a partial failure leaves
   nothing visible to users.

9. **Verify one archive** before announcing:

   ```sh
   tar tzf mult-0.2.0-x86_64-unknown-linux-musl.tar.gz
   sha256sum -c mult-0.2.0-x86_64-unknown-linux-musl.tar.gz.sha256
   ```

## If something goes wrong

The tag is the trigger, so recovery is: delete the draft release, delete the
tag locally and remotely, fix, and re-tag.

```sh
gh release delete v0.2.0 --yes
git push --delete origin v0.2.0
git tag -d v0.2.0
```

Deleting a tag that was already published as a non-draft release is disruptive
— people may have downloaded it. Prefer cutting `v0.2.1`.

## Installing from a release

```sh
tar xzf mult-0.2.0-x86_64-unknown-linux-musl.tar.gz
cd mult-0.2.0-x86_64-unknown-linux-musl
install -m755 mult mult-server ~/.local/bin/
```

Both binaries, same directory. See above for why.

## Not done yet

Deliberately out of scope for the current workflow, and worth deciding before a
`1.0`:

- **Signing and provenance.** No signed tags, no build attestation, no SBOM.
- **Reproducible builds.** The archives are not byte-reproducible.
- **Publishing to crates.io.** Neither crate sets `publish = false`; nothing has
  been published. `mult-protocol` is the one that would plausibly want to be.
- **Upgrade tests.** Nothing exercises "old state file, new binary" or
  "old client, new daemon" as part of the release. The state migration has
  fixtures; the protocol boundary does not.
- **Distribution packaging.** No Homebrew tap, no AUR, no nixpkgs entry. The
  flake in this repository is the Nix path.

These are carried in [ROADMAP.md](ROADMAP.md) under the release-intent decision.
