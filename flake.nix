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
            # Kept in step with `[workspace.package] version` in Cargo.toml by
            # `just version-check`.
            version = "0.1.0";
            src = ./.;
            cargoLock.lockFile = ./Cargo.lock;
            # The sandbox has no `/bin/sh`, and two things need a shell there.
            # `MULT_TEST_SHELL` is read by the integration harness when it
            # spawns its own daemon; `SHELL` is what the *code* reads
            # (`mult-server.rs`'s `default_shell`, `pty.rs`'s `login_shell`) and
            # what any unit test that spawns a pane therefore falls back to.
            # Setting only the first left `nix flake check` passing by accident,
            # because no unit test happened to spawn a pane yet (S10).
            MULT_TEST_SHELL = "${pkgs.runtimeShell}";
            SHELL = "${pkgs.runtimeShell}";
            # buildRustPackage runs inside a Nix sandbox where PTY-backed
            # integration tests cannot reliably create interactive devices, so
            # this job's `tests/pty_integration.rs` run proves nothing. The
            # `cargo` job in .github/workflows/ci.yml sets
            # `MULT_REQUIRE_PTY_INTEGRATION=1` and greps the suite's execution
            # sentinel, so the coverage this skip gives up is provably taken
            # elsewhere (S9). Do not set the skip anywhere a PTY is available.
            MULT_SKIP_PTY_INTEGRATION = "1";
            # Runtime-file tests need a writable base inside the sandbox rather
            # than inheriting a host XDG_RUNTIME_DIR.
            XDG_RUNTIME_DIR = "/tmp";
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
              # `cargo-deny` is the supply-chain gate (`just deny`); it
              # supersedes the standalone `cargo audit` that used to run in
              # `just ci`. See deny.toml.
              cargo-deny
              cargo-llvm-cov
              just
            ];

            RUST_BACKTRACE = "1";
            RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";

            shellHook = ''
              export PATH="$HOME/.cargo/bin:$PATH"
            '';
          };
        });
    };
}
