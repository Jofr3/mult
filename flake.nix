{
  description = "mult — AI agent multiplexer TUI";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs = { self, nixpkgs }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];
      forAllSystems = nixpkgs.lib.genAttrs systems;
    in
    {
      packages = forAllSystems (system:
        let pkgs = nixpkgs.legacyPackages.${system};
        in {
          default = pkgs.rustPlatform.buildRustPackage {
            pname = "mult";
            version = "0.1.0";
            src = ./.;
            cargoLock.lockFile = ./Cargo.lock;
            MULT_TEST_SHELL = "${pkgs.runtimeShell}";
            # The Nix sandbox has a `/dev/ptmx` symlink but no `devpts` mounted
            # behind it, so `openpty` fails with ENOENT before any of mult's own
            # code runs. This opts out of every PTY-backed test — the integration
            # suite *and* the daemon's own dispatch tests, which spawn real panes
            # (G15). It is the only thing that sets this variable; everywhere a
            # PTY can be allocated, those tests run and must pass.
            MULT_SKIP_PTY_INTEGRATION = "1";
          };
        });

      apps = forAllSystems (system: {
        default = {
          type = "app";
          program = "${self.packages.${system}.default}/bin/mult";
          meta.description = "Run the mult TUI client";
        };
        server = {
          type = "app";
          program = "${self.packages.${system}.default}/bin/mult-server";
          meta.description = "Run the persistent mult PTY server";
        };
      });

      checks = forAllSystems (system: {
        default = self.packages.${system}.default;
      });

      devShells = forAllSystems (system:
        let pkgs = nixpkgs.legacyPackages.${system};
        in {
          default = pkgs.mkShell {
            packages = with pkgs; [
              cargo
              clippy
              rust-analyzer
              rustc
              rustfmt

              cargo-watch
              # cargo-deny covers advisories, licenses, bans and sources.
              # cargo-audit used to sit alongside it and only re-checked a
              # subset of the advisories section, so it was dropped (H10).
              cargo-deny
              cargo-llvm-cov
              # `just fuzz-build` / `cargo fuzz run`; both need a nightly rustc,
              # which is not in this shell — see CONTRIBUTING.md.
              cargo-fuzz
              just
            ];

            RUST_BACKTRACE = "1";
            RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";

            # `just coverage` needs llvm-cov/llvm-profdata. The rustup
            # `llvm-tools-preview` component does not exist in this shell, so
            # point cargo-llvm-cov at nixpkgs' LLVM instead — it is the same
            # version this shell's rustc is built against, which is the thing
            # that has to match for the profile data to be readable.
            LLVM_COV = "${pkgs.llvmPackages.llvm}/bin/llvm-cov";
            LLVM_PROFDATA = "${pkgs.llvmPackages.llvm}/bin/llvm-profdata";

            shellHook = ''
              export PATH="$HOME/.cargo/bin:$PATH"
            '';
          };
        });
    };
}
