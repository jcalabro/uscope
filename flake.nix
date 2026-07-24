{
  description = "uscope Linux debugger development environment";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";
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
      goStable = pkgs.go.overrideAttrs (_final: _previous: {
        version = "1.26.5";
        src = pkgs.fetchurl {
          url = "https://go.dev/dl/go1.26.5.src.tar.gz";
          hash = "sha256-SVvkvIcXasVnOS5bQRar2YRm0z17SdQedkzMaXay3EI=";
        };
      });
    in {
      devShells.${system}.default = pkgs.mkShell {
        NIX_HARDENING_ENABLE = "";
        RUSTFLAGS = "-C link-arg=-Wl,--dynamic-linker=${pkgs.glibc}/lib/ld-linux-x86-64.so.2";
        packages = with pkgs; [
          rust
          cargo-fuzz
          cargo-nextest
          just
          gcc
          clang
          gdb
          lldb
          goStable
          zig
          pkg-config
        ];
      };
    };
}
