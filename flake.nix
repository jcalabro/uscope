{
  description = "uscope Linux debugger development environment";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.11";
    rust-overlay.url = "github:oxalica/rust-overlay";
  };

  outputs = { self, nixpkgs, rust-overlay }:
    let
      system = "x86_64-linux";
      pkgs = import nixpkgs {
        inherit system;
        overlays = [ rust-overlay.overlays.default ];
      };
      rust = pkgs.rust-bin.nightly."2026-07-11".default.override {
        extensions = [ "clippy" "rust-src" "rustfmt" ];
      };
    in {
      devShells.${system}.default = pkgs.mkShell {
        NIX_HARDENING_ENABLE = "";
        RUSTFLAGS = "-C link-arg=-Wl,--dynamic-linker=${pkgs.glibc}/lib/ld-linux-x86-64.so.2";
        packages = with pkgs; [
          rust
          cargo-nextest
          just
          gcc
          clang
          lldb
          pkg-config
        ];
      };
    };
}
