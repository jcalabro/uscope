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
      # GCC and Clang targeting musl instead of glibc, so fixtures cover both
      # C libraries' thread layouts. Wrapping them keeps the cross toolchain's
      # environment out of the shell's native one.
      musl64 = pkgs.pkgsCross.musl64;
      muslGcc = pkgs.writeShellScriptBin "musl-gcc" ''
        exec ${musl64.stdenv.cc}/bin/x86_64-unknown-linux-musl-gcc "$@"
      '';
      muslClang = pkgs.writeShellScriptBin "musl-clang" ''
        exec ${musl64.buildPackages.clang}/bin/x86_64-unknown-linux-musl-clang "$@"
      '';
      # The crates the tokio fixtures depend on, exactly as their lockfile
      # names them, so that building them fetches nothing.
      tokioFixtureCrates = pkgs.rustPlatform.importCargoLock {
        lockFile = ./tests/fixtures/rust/tokio/Cargo.lock;
      };
      default = pkgs.mkShell {
        NIX_HARDENING_ENABLE = "";
        # glibc's static libraries, which only statically linked fixtures link
        # against: on the default search path they would shadow the shared C
        # library for every program.
        GLIBC_STATIC_LIBRARIES = "${pkgs.glibc.static}/lib";
        # The vendored crates scripts/build-test-programs.sh builds the tokio
        # fixtures against, offline.
        USCOPE_FIXTURE_CRATES = "${tokioFixtureCrates}";
        # The web UI's end-to-end tests drive the browsers nixpkgs builds,
        # which must match the pinned @playwright/test exactly.
        PLAYWRIGHT_BROWSERS_PATH = "${pkgs.playwright-driver.browsers}";
        PLAYWRIGHT_SKIP_VALIDATE_HOST_REQUIREMENTS = "true";
        PLAYWRIGHT_DRIVER_VERSION = pkgs.playwright-driver.version;
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
          muslGcc
          muslClang
          gdb
          lldb
          # Profilers for the `just profile-*` recipes.
          perf
          valgrind
          goStable
          zig
          pkg-config
          util-linux
          nodejs_24
          pnpm
          biome
        ];
      };
    in {
      devShells.${system} = {
        inherit default;
        # The default shell with the profilers people read at a screen, kept
        # out of it for their size: Tracy, whose version the `tracy` feature's
        # client must match (`just tracy-check`), samply, hotspot, heaptrack,
        # hyperfine, poop, and KCachegrind.
        profile = default.overrideAttrs (previous: {
          nativeBuildInputs = previous.nativeBuildInputs ++ (with pkgs; [
            tracy
            samply
            hotspot
            heaptrack
            hyperfine
            poop
            kdePackages.kcachegrind
          ]);
          TRACY_VERSION = pkgs.tracy.version;
        });
      };
    };
}
