{
  description = "lz4-oxide dev shell — Rust port of liblz4";

  inputs = {
    nixos-config.url = "path:/home/hridesh/nix-config";
    nixpkgs.follows = "nixos-config/nixpkgs";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs =
    {
      self,
      nixpkgs,
      rust-overlay,
      flake-utils,
      ...
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ rust-overlay.overlays.default ];
        };

        # Pin to stable; Cargo.toml requires >= 1.70.
        rustToolchain = pkgs.rust-bin.stable.latest.default.override {
          extensions = [
            "rust-src"
            "rust-analyzer"
            "clippy"
            "rustfmt"
          ];
        };
      in
      {
        devShells.default = pkgs.mkShell {
          name = "lz4-oxide";

          packages = [
            rustToolchain

            # C compiler — build.rs compiles cstub/*.c and upstream headers
            pkgs.gcc
            pkgs.gnumake

            # nm (abi-check / gen-ffi), addr2line, etc.
            pkgs.binutils

            # stdbuf (fuzzer invocation in CLAUDE.md), sha256sum
            pkgs.coreutils

            # python3 for tools/gen_ffi.py (make gen-ffi)
            pkgs.python3

            # pkg-config in case any future build.rs needs it
            pkgs.pkg-config
          ];

          # Let build.rs (cc crate) find gcc
          CC = "gcc";

          shellHook = ''
            echo "lz4-oxide dev shell"
            echo "  rustc $(rustc --version)"
            echo "  cargo $(cargo --version)"
            echo "  gcc   $(gcc --version | head -1)"
            echo ""
            echo "Targets: make link-check  make test  make abi-check"
            echo "Fuzzer:  stdbuf -oL ./upstream/tests/fuzzer -i1"
          '';
        };
      }
    );
}
