{
  description = "Aven development shell";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      nixpkgs,
      rust-overlay,
      ...
    }:
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
      devShells = forAllSystems (
        system:
        let
          pkgs = import nixpkgs {
            inherit system;
            overlays = [ rust-overlay.overlays.default ];
          };
          # Same pin as rust-toolchain.toml / CI clippy. Extra components are
          # local-only; GitHub Actions installs clippy+rustfmt via the action.
          rust = pkgs.rust-bin.stable."1.98.1".default.override {
            extensions = [
              "rust-src"
              "rust-analyzer"
            ];
          };
          # `crates/aven-cli/tests/cli_args.rs` drives a real Readline session to
          # check generated completions. nixpkgs' plain `bash` is built without
          # readline or programmable completion, and mkShell puts it on PATH
          # ahead of the host's, so `complete` disappears and four tests fail
          # inside the shell while passing outside it. CI runs on a full bash.
          shells = [
            pkgs.bashInteractive
            pkgs.fish
          ];
        in
        {
          default = pkgs.mkShell {
            packages = [ rust ] ++ shells;
          };
          # MSRV gate: `cargo check --workspace --all-targets` only, matching
          # ci.yml's `dtolnay/rust-toolchain@1.91.0` job. rust-toolchain.toml
          # pins 1.98.1, which rustup would otherwise honour over any default.
          msrv = pkgs.mkShell {
            packages = [ pkgs.rust-bin.stable."1.91.0".minimal ] ++ shells;
            RUSTUP_TOOLCHAIN = "1.91.0";
          };
        }
      );
    };
}
