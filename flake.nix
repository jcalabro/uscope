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
      goStable = pkgs.go.overrideAttrs (_final: previous: {
        version = "1.27.1";
        src = pkgs.fetchurl {
          url = "https://go.dev/dl/go1.27.1.src.tar.gz";
          hash = "sha256-TkCKuuEm2Ra2FkYnGT8sVPDjyhMS1pO4bbRfhiqyOLE=";
        };
        # The patch that relaxes module vendoring for nixpkgs' Go builders no
        # longer applies to 1.27, and nothing here builds with them.
        patches = builtins.filter
          (patch: !(pkgs.lib.hasInfix "go_no_vendor_checks" (toString patch)))
          previous.patches;
      });
      # Builds C++ against LLVM's libc++ instead of libstdc++, so fixtures
      # cover both standard libraries' layouts.
      libcxx = pkgs.llvmPackages.libcxx;
      clangLibcxx = pkgs.writeShellScriptBin "clang++-libc++" ''
        exec ${pkgs.clang}/bin/clang++ -stdlib=libc++ -nostdinc++ \
          -isystem ${libcxx.dev}/include/c++/v1 \
          -L${libcxx}/lib -Wl,-rpath,${libcxx}/lib "$@"
      '';
    in {
      devShells.${system}.default = pkgs.mkShell {
        NIX_HARDENING_ENABLE = "";
        RUSTFLAGS = "-C link-arg=-fuse-ld=mold -C link-arg=-Wl,--dynamic-linker=${pkgs.glibc}/lib/ld-linux-x86-64.so.2";
        packages = with pkgs; [
          rust
          cargo-fuzz
          cargo-nextest
          just
          gcc
          mold
          clang
          clangLibcxx
          gdb
          lldb
          goStable
          zig
          pkg-config
          util-linux
        ];
      };
    };
}
