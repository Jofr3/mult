# Releasing

`mult` ships two binaries that must travel together, and the version is written
in more than one file. This is the checklist.

## Where the version lives

| File | How it is set |
| --- | --- |
| `Cargo.toml` | `[workspace.package] version` — **the source of truth** |
| `crates/protocol/Cargo.toml` | inherited (`version.workspace = true`); nothing to edit |
| `flake.nix` | `version = "…";` in the package definition — hand-maintained |
| `extensions/package.json` | `"version": "…"` — hand-maintained |

`just version-check` reads `Cargo.toml` as the reference and compares the two
hand-maintained copies (`flake.nix`, `extensions/package.json`) against it,
failing on a mismatch. It runs as part of `just ci`, so a forgotten bump fails the build
rather than shipping. It is plain text matching, so it needs no network.

`PROTOCOL_VERSION` in `crates/protocol/src/lib.rs` is **not** the release
version and is deliberately independent: it is bumped whenever the wire format
changes, which may happen several times within one release or not at all.

## Checklist

1. **Decide the version.** Semver over the user-visible surface: the CLI, the
   config schema, the state schema, the keybindings.

2. **Bump it in the three places above.**

   ```sh
   just version-check    # fails until all three agree
   ```

3. **Update `CHANGELOG.md`.**
   - Move everything under `[Unreleased]` into a new `[X.Y.Z] - YYYY-MM-DD`
     section, keeping the Added/Changed/Fixed/Security grouping.
   - Leave an empty `[Unreleased]` heading behind.
   - Add the two link definitions at the bottom — the `[Unreleased]` compare link
     now points at the new tag, and the new version gets its own. Without a link
     definition a `[X.Y.Z]` heading renders as literal brackets.

4. **Check the MSRV is still honest.** `Cargo.toml` declares
   `rust-version = "1.88"` and CI has a job pinned to exactly that; if that job
   is failing, either fix the code or raise the declared version — in
   `[workspace.package]` **and** in the CI job — before tagging.

5. **Run the gate.**

   ```sh
   just ci
   just coverage
   cargo test --test pty_integration
   ```

6. **Commit** the bump on its own, e.g. `chore(release): 0.1.0`.

7. **Tag and push.** The tag must be `vX.Y.Z`, or a pre-release
   `vX.Y.Z-<suffix>` — the release workflow triggers on those two patterns, and
   `gh release create --verify-tag` requires the tag to exist on the remote
   first.

   ```sh
   git tag -a v0.1.0 -m "v0.1.0"
   git push origin main
   git push origin v0.1.0
   ```

8. **Watch the release workflow.** `.github/workflows/release.yml` builds four
   archives and publishes a GitHub Release with a `SHA256SUMS` file:

   | Archive | Built on |
   | --- | --- |
   | `mult-X.Y.Z-x86_64-unknown-linux-gnu.tar.gz` | ubuntu-latest |
   | `mult-X.Y.Z-x86_64-unknown-linux-musl.tar.gz` | ubuntu-latest + `musl-tools` |
   | `mult-X.Y.Z-x86_64-apple-darwin.tar.gz` | macos-latest |
   | `mult-X.Y.Z-aarch64-apple-darwin.tar.gz` | macos-latest |

   Every archive contains **both `mult` and `mult-server`** plus the README,
   CHANGELOG and both licences. This is not optional packaging taste: the client
   autospawns the daemon from a path next to its own binary and verifies that
   binary's ownership and mode before executing it, so an archive with one of the
   two leaves autospawn permanently broken.

9. **Smoke-test one archive** before announcing:

   ```sh
   tar xzf mult-X.Y.Z-x86_64-unknown-linux-gnu.tar.gz
   cd mult-X.Y.Z-x86_64-unknown-linux-gnu
   ./mult --version
   ./mult-server --version
   ```

10. **Update the backlog and plan** if the release closes tracked items.

## Dry run without tagging

The release workflow accepts `workflow_dispatch`. Running it manually builds all
four archives and uploads them as workflow artifacts without creating a GitHub
Release, which is the way to test a matrix change before cutting a tag.

## If a release goes wrong

Do not move a published tag. Cut the next patch version instead — the archives
and the `SHA256SUMS` for a tag are immutable once anyone has downloaded them.
