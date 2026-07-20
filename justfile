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

# Run dependency advisory checks. cargo-audit is available in `nix develop`.
audit:
    command -v cargo-audit >/dev/null || { echo "cargo-audit is required; use nix develop or install cargo-audit"; exit 127; }
    cargo audit -D warnings

# Strict local CI gate.
ci: fmt-check lint test audit

# Re-run cargo check whenever files change.
watch:
    cargo watch -x "check --locked --workspace --all-targets --all-features" -x "test --locked --workspace --all-targets --all-features"

# Build using the Nix package definition.
nix-build:
    nix build

# Run flake checks.
nix-check:
    nix flake check
