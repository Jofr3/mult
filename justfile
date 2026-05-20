set shell := ["bash", "-eu", "-o", "pipefail", "-c"]

# Run the TUI client.
run:
    cargo run

# Run the persistent terminal server.
server:
    cargo run --bin mult-server

# Fast feedback during development.
check:
    cargo check --workspace

# Run unit tests.
test:
    cargo test --workspace

# Format Rust sources and Nix files when nixpkgs-fmt is available.
fmt:
    cargo fmt
    if command -v nixpkgs-fmt >/dev/null; then nixpkgs-fmt flake.nix; fi

# Lint with warnings treated as errors.
lint:
    cargo clippy --workspace --all-targets --all-features -- -D warnings

# Run dependency advisory checks when cargo-audit is installed.
audit:
    if command -v cargo-audit >/dev/null; then cargo audit; else echo "cargo-audit not installed; skipping"; fi

# Re-run cargo check whenever files change.
watch:
    cargo watch -x check -x test

# Build using the Nix package definition.
nix-build:
    nix build

# Run flake checks.
nix-check:
    nix flake check
