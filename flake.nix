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
            # buildRustPackage runs inside a Nix sandbox where PTY-backed
            # integration tests cannot reliably create interactive devices.
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
              cargo-audit
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
