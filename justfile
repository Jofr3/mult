set shell := ["bash", "-eu", "-o", "pipefail", "-c"]

# Run the TUI client.
run:
    cargo run --locked

# Run the persistent terminal server.
server:
    cargo run --locked --bin mult-server

# Fast feedback during development.
check:
    cargo check --locked --workspace --all-targets --all-features

# Run unit tests.
test:
    cargo test --locked --workspace --all-targets --all-features

# Format Rust sources and Nix files when nixpkgs-fmt is available.
fmt:
    cargo fmt --all
    if command -v nixpkgs-fmt >/dev/null; then nixpkgs-fmt flake.nix; fi

# Check formatting without modifying files.
fmt-check:
    cargo fmt --all -- --check

# Lint with warnings treated as errors.
lint:
    cargo clippy --locked --workspace --all-targets --all-features -- -D warnings

# This is the single dependency gate. `deny.toml` supersedes the standalone
# `cargo audit`, which used to run in `ci` and duplicated the advisory half of
# it. cargo-deny is available in `nix develop`.

# Supply-chain gate: advisories, licenses, bans, and crate sources.
deny:
    command -v cargo-deny >/dev/null || { echo "cargo-deny is required; use nix develop or install cargo-deny"; exit 127; }
    cargo deny --locked check

# Degrades to a notice rather than a failure when npm or the installed
# dependencies are missing, so `ci` still completes offline and on a fresh
# clone. CI runs the real thing in its own job after `npm ci`.

# Typecheck the bundled pi status extension (`tsc --noEmit`).
typecheck:
    #!/usr/bin/env bash
    set -euo pipefail
    if ! command -v npm >/dev/null 2>&1; then
        echo "note: skipping extension typecheck (npm not found)"
        exit 0
    fi
    if [ ! -d extensions/node_modules ]; then
        echo "note: skipping extension typecheck (run 'npm ci --ignore-scripts' in extensions/)"
        exit 0
    fi
    npm --prefix extensions run typecheck

# `crates/protocol` inherits `[workspace.package] version`, so the remaining
# copies are the workspace manifest, the Nix package, and the bundled
# extension's package.json. Pass an expected version (the release workflow
# passes the git tag without its leading `v`) to also pin it to that value.

# Assert every place the release version is written still agrees.
version-check expected="":
    #!/usr/bin/env bash
    set -euo pipefail
    version="$(sed -n '/^\[workspace\.package\]/,/^\[/s/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)"
    if [ -z "$version" ]; then
        echo "version-check: no [workspace.package] version in Cargo.toml" >&2
        exit 1
    fi
    status=0
    if ! grep -q '^\s*version\.workspace = true' crates/protocol/Cargo.toml; then
        echo "version-check: crates/protocol/Cargo.toml must inherit 'version.workspace = true'" >&2
        status=1
    fi
    if ! grep -q "version = \"$version\";" flake.nix; then
        echo "version-check: flake.nix does not declare version $version" >&2
        status=1
    fi
    if ! grep -q "\"version\": \"$version\"," extensions/package.json; then
        echo "version-check: extensions/package.json does not declare version $version" >&2
        status=1
    fi
    if [ -n '{{ expected }}' ] && [ '{{ expected }}' != "$version" ]; then
        echo "version-check: expected {{ expected }} but the workspace declares $version" >&2
        status=1
    fi
    if [ "$status" -eq 0 ]; then
        echo "version-check: $version"
    fi
    exit "$status"

# Reports a number; no threshold is enforced locally (CI has a floor well under
# the baseline). cargo-llvm-cov is available in `nix develop`.

# Line and region coverage for the workspace.
coverage:
    command -v cargo-llvm-cov >/dev/null || { echo "cargo-llvm-cov is required; use nix develop or install cargo-llvm-cov"; exit 127; }
    cargo llvm-cov --locked --workspace --all-features --summary-only

# This is the same set GitHub Actions runs per platform (Linux and macOS); CI
# additionally runs the MSRV check, coverage, a real `npm ci` typecheck, and
# `nix flake check` in their own jobs.

# Strict local CI gate.
ci: version-check fmt-check lint test deny typecheck

# Formatting only, on purpose: a hook slow enough to be worth bypassing gets
# bypassed. Everything else is `just ci`.

# Install a pre-commit hook running `cargo fmt --all -- --check`.
install-hooks:
    #!/usr/bin/env bash
    set -euo pipefail
    hooks="$(git rev-parse --git-path hooks)"
    mkdir -p "$hooks"
    printf '%s\n' \
        '#!/bin/sh' \
        '# Installed by `just install-hooks`. Formatting only — run `just ci` for the rest.' \
        'exec cargo fmt --all -- --check' > "$hooks/pre-commit"
    chmod +x "$hooks/pre-commit"
    echo "installed $hooks/pre-commit (cargo fmt --all -- --check)"

# Re-run cargo check whenever files change.
watch:
    cargo watch -x "check --locked --workspace --all-targets --all-features" -x "test --locked --workspace --all-targets --all-features"

# Build using the Nix package definition.
nix-build:
    nix build

# Run flake checks.
nix-check:
    nix flake check
