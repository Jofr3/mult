set shell := ["bash", "-eu", "-o", "pipefail", "-c"]

# Run the TUI.
run:
    cargo run

# Fast feedback during development.
check:
    cargo check

# Run unit tests.
test:
    cargo test

# Format Rust sources and Nix files when nixpkgs-fmt is available.
fmt:
    cargo fmt
    if command -v nixpkgs-fmt >/dev/null; then nixpkgs-fmt flake.nix; fi

# Lint with warnings treated as errors.
lint:
    cargo clippy --all-targets --all-features -- -D warnings

# Re-run cargo check whenever files change.
watch:
    cargo watch -x check -x test

# Build using the Nix package definition.
nix-build:
    nix build

# Run flake checks.
nix-check:
    nix flake check
