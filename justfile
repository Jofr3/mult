set shell := ["bash", "-eu", "-o", "pipefail", "-c"]

# Run the TUI client.
run:
    cargo run

# Run the persistent terminal server.
server:
    cargo run --bin mult-server

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

# Dependency policy check: advisories (the RustSec database), licenses, banned
# and duplicate crates, and crate sources. This is the whole supply-chain gate —
# `cargo audit` used to run alongside it and checked a strict subset of the
# `advisories` section, so it was removed rather than kept as a second opinion
# that could only ever agree (H10). cargo-deny is available in `nix develop`.
deny:
    command -v cargo-deny >/dev/null || { echo "cargo-deny is required; use nix develop or install cargo-deny"; exit 127; }
    cargo deny check

# Typecheck the bundled status extension. Mirrors the `extension` job in CI.
# Skipped with a notice when npm or the installed dependencies are absent, so
# the gate still runs offline and without Node.
typecheck:
    if ! command -v npm >/dev/null; then \
        echo "skipping extension typecheck: npm is not installed"; \
    elif [ ! -d extensions/node_modules ]; then \
        echo "skipping extension typecheck: extensions/node_modules is missing (run 'npm ci' in extensions/)"; \
    else \
        npm --prefix extensions run typecheck; \
    fi

# Check that the release version agrees everywhere it is written (H17).
# `Cargo.toml`'s `[workspace.package] version` is the source of truth; the
# protocol crate inherits it, so only `flake.nix` and `extensions/package.json`
# can drift. Pure text matching, so it needs no network and no cargo build.
version-check:
    #!/usr/bin/env bash
    set -euo pipefail
    cargo_version="$(sed -n '/^\[workspace.package\]/,/^\[/p' Cargo.toml \
        | sed -n 's/^version = "\(.*\)"$/\1/p' | head -1)"
    if [ -z "$cargo_version" ]; then
        echo "version-check: could not read [workspace.package] version from Cargo.toml" >&2
        exit 1
    fi
    status=0
    check() {
        if [ "$2" != "$cargo_version" ]; then
            echo "version-check: $1 has '$2', expected '$cargo_version'" >&2
            status=1
        fi
    }
    check flake.nix "$(sed -n 's/^ *version = "\(.*\)";$/\1/p' flake.nix | head -1)"
    check extensions/package.json \
        "$(sed -n 's/^ *"version": "\(.*\)",$/\1/p' extensions/package.json | head -1)"
    if [ "$status" -eq 0 ]; then
        echo "version-check: $cargo_version everywhere"
    else
        echo "version-check: see docs/RELEASING.md for the full checklist" >&2
    fi
    exit "$status"

# Line/region coverage over the workspace (H9). cargo-llvm-cov is available in
# `nix develop`. The floor is a regression guard set below the measured number,
# not a target: it fails a change that drops coverage sharply, and does not ask
# unrelated work to raise it.
coverage:
    command -v cargo-llvm-cov >/dev/null || { echo "cargo-llvm-cov is required; use nix develop or install cargo-llvm-cov"; exit 127; }
    cargo llvm-cov --locked --workspace --all-features --summary-only --fail-under-lines 85

# Write an HTML coverage report to target/llvm-cov/html and print its path.
coverage-html:
    command -v cargo-llvm-cov >/dev/null || { echo "cargo-llvm-cov is required; use nix develop or install cargo-llvm-cov"; exit 127; }
    cargo llvm-cov --locked --workspace --all-features --html
    echo "report: target/llvm-cov/html/index.html"

# Install the repository's git hooks into .git/hooks (H16).
#
# The hook runs `cargo fmt --all -- --check` and nothing else. That is a
# deliberate ceiling: a pre-commit hook that runs clippy or the test suite costs
# minutes per commit and gets bypassed with `--no-verify` within a day, at which
# point it stops catching anything. Formatting is the one check that is both
# fast and mechanical. Everything else belongs in `just ci`.
install-hooks:
    #!/usr/bin/env bash
    set -euo pipefail
    hooks="$(git rev-parse --git-path hooks)"
    mkdir -p "$hooks"
    cat > "$hooks/pre-commit" <<'HOOK'
    #!/usr/bin/env bash
    # Installed by `just install-hooks`. Formatting only — see the justfile for why.
    set -euo pipefail
    if ! command -v cargo >/dev/null; then
        exit 0
    fi
    if ! cargo fmt --all -- --check; then
        echo >&2
        echo "pre-commit: formatting check failed. Run 'cargo fmt --all' and stage the result." >&2
        echo "pre-commit: to commit anyway, use 'git commit --no-verify'." >&2
        exit 1
    fi
    HOOK
    chmod +x "$hooks/pre-commit"
    echo "installed $hooks/pre-commit (cargo fmt --all -- --check)"

# Build the fuzz targets without running them (G3). See CONTRIBUTING.md for how
# to run a campaign.
#
# The nightly toolchain's bin directory is put on PATH by hand rather than using
# `cargo +nightly` or `rustup run nightly`. Both of those go through the rustup
# proxy, and inside `nix develop` the first `cargo` on PATH is nixpkgs' own,
# which rejects `+toolchain` and shadows the toolchain `rustup run` selects — so
# cargo-fuzz's own inner `cargo build` ends up on stable and fails on the
# `-Z` flags. Resolving the real binary makes this work in both shells, and
# bypasses the 1.94 pin in rust-toolchain.toml, which would otherwise win.
fuzz-build:
    #!/usr/bin/env bash
    set -euo pipefail
    command -v cargo-fuzz >/dev/null || { echo "cargo-fuzz is required; use nix develop or install cargo-fuzz"; exit 127; }
    command -v rustup >/dev/null || { echo "cargo-fuzz needs a nightly rustc, which needs rustup; see CONTRIBUTING.md"; exit 127; }
    nightly_cargo="$(rustup which --toolchain nightly cargo 2>/dev/null || true)"
    if [ -z "$nightly_cargo" ]; then
        echo "no nightly toolchain; run 'rustup toolchain install nightly'" >&2
        exit 127
    fi
    cd fuzz
    PATH="$(dirname "$nightly_cargo"):$PATH" cargo fuzz build

# Strict local CI gate. Mirrors the GitHub Actions jobs.
ci: fmt-check version-check lint test deny typecheck

# Re-run cargo check whenever files change.
watch:
    cargo watch -x check -x test

# Build using the Nix package definition.
nix-build:
    nix build

# Run flake checks.
nix-check:
    nix flake check
