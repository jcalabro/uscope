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
        extensions = [ "clippy" "miri" "rust-src" "rustfmt" ];
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
      # Ubuntu's builds of glibc: the loader and libraries the TLS fixtures
      # built for them run on, in lib, and the startup files they link with,
      # in dev. 18.04's and 20.04's describe their thread structures in
      # libpthread; 22.04's in libc, as every glibc since 2.34 does.
      ubuntuPackage = name: version: hash: pkgs.fetchurl {
        urls = [
          "https://launchpad.net/ubuntu/+archive/primary/+files/${name}_${version}_amd64.deb"
          "http://archive.ubuntu.com/ubuntu/pool/main/g/glibc/${name}_${version}_amd64.deb"
        ];
        inherit hash;
      };
      ubuntuGlibc = { version, package, runtime, development }: pkgs.runCommand "ubuntu-glibc-${version}" {
        runtime = ubuntuPackage "libc6" package runtime;
        development = ubuntuPackage "libc6-dev" package development;
        nativeBuildInputs = [ pkgs.binutils pkgs.zstd ];
      } ''
        unpack() { mkdir "$2"; (cd "$2" && ar x "$1" && tar xf data.tar.*); }
        unpack "$runtime" runtime
        unpack "$development" development
        mkdir -p "$out/dev"
        mv runtime/lib/x86_64-linux-gnu "$out/lib"
        cd development/usr/lib/x86_64-linux-gnu
        for file in Scrt1.o crti.o crtn.o libc_nonshared.a libpthread_nonshared.a; do
          if [ -e "$file" ]; then cp "$file" "$out/dev"; fi
        done
      '';
      ubuntuGlibcs = pkgs.linkFarm "ubuntu-glibc" (map
        (release: { name = release.version; path = ubuntuGlibc release; })
        [
          {
            version = "2.27";
            package = "2.27-3ubuntu1.6";
            runtime = "sha256-W4E9RRJJHNj7MLLrkZI0nudf/jyfIa4TbMeyQp9ww7I=";
            development = "sha256-lq59LPOPBUfKPMyzNEyIAv31vNccS22a7QlVisjMh9s=";
          }
          {
            version = "2.31";
            package = "2.31-0ubuntu9.18";
            runtime = "sha256-GyKBqsSTXf6h+J38GeRF/btFMDICr2BnnMuL8DXwgaA=";
            development = "sha256-EQKZAPIxW5POSHeSrhvxXwkn5eUIgMCR5qRAsU1OFko=";
          }
          {
            version = "2.35";
            package = "2.35-0ubuntu3.15";
            runtime = "sha256-eeNSViJ+FqYHwVTN6412/xLSDjHeKG6n/ZrTuW/gRS0=";
            development = "sha256-zlGuIzqfgAoEDeLQzVmD74b1WMYjs0LICUIwHX9FP0g=";
          }
        ]);
      # GNU Fortran, alone: its wrapper's own gcc and binutils would shadow
      # the shell's.
      gfortran = pkgs.writeShellScriptBin "gfortran" ''
        exec ${pkgs.gfortran15}/bin/gfortran "$@"
      '';
      # LDC, alone: its package would add its runtime's headers and
      # libraries to every C compile and link in the shell.
      ldc2 = pkgs.writeShellScriptBin "ldc2" ''
        exec ${pkgs.ldc}/bin/ldc2 "$@"
      '';
      # GNAT's gnatmake, which runs its own GCC, binder, and linker; the
      # shell's GCC compiles no Ada.
      gnatmake = pkgs.writeShellScriptBin "gnatmake" ''
        PATH=${pkgs.gnat15}/bin:$PATH exec gnatmake "$@"
      '';
      # The crates the tokio fixtures depend on, exactly as their lockfiles
      # name them, one for each tokio release they are built with, so that
      # building them fetches nothing.
      tokioFixtureCrates = pkgs.symlinkJoin {
        name = "tokio-fixture-crates";
        paths = map (lockFile: pkgs.rustPlatform.importCargoLock { inherit lockFile; }) [
          ./tests/fixtures/rust/tokio/Cargo.lock
          ./tests/fixtures/rust/tokio/tokio-1.52/Cargo.lock
        ];
      };
      default = pkgs.mkShell {
        NIX_HARDENING_ENABLE = "";
        # glibc's static libraries, which only statically linked fixtures link
        # against: on the default search path they would shadow the shared C
        # library for every program.
        GLIBC_STATIC_LIBRARIES = "${pkgs.glibc.static}/lib";
        # Ubuntu's glibc builds, one directory for each version, holding its
        # lib and dev directories.
        USCOPE_UBUNTU_GLIBC = "${ubuntuGlibcs}";
        # The vendored crates scripts/build-test-programs.sh builds the tokio
        # fixtures against, offline.
        USCOPE_FIXTURE_CRATES = "${tokioFixtureCrates}";
        # The web UI's end-to-end tests drive the browsers nixpkgs builds,
        # which must match the pinned @playwright/test exactly.
        PLAYWRIGHT_BROWSERS_PATH = "${pkgs.playwright-driver.browsers}";
        PLAYWRIGHT_SKIP_VALIDATE_HOST_REQUIREMENTS = "true";
        PLAYWRIGHT_DRIVER_VERSION = pkgs.playwright-driver.version;
        RUSTFLAGS = "-C link-arg=-fuse-ld=mold -C link-arg=-Wl,--dynamic-linker=${pkgs.glibc}/lib/ld-linux-x86-64.so.2";
        # build.rs compiles the syscall tracer's BPF programs with clang
        # itself, not its wrapper, which adds host flags that a BPF target
        # rejects, against libbpf's BPF headers and the kernel's UAPI
        # headers, whose syscall numbers also generate src/syscalls' tables.
        USCOPE_BPF_CLANG = "${pkgs.llvmPackages.clang-unwrapped}/bin/clang";
        USCOPE_BPF_INCLUDE = "${pkgs.libbpf}/include:${pkgs.linuxHeaders}/include";
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
          # Points fixtures built for Ubuntu's glibc at its loader and
          # libraries.
          patchelf
          gdb
          lldb
          # Shares fixtures' debug information through supplementary files,
          # as distributions do.
          dwz
          # The memory cap tests run under asks the user's systemd for a scope.
          systemd
          git
          # The syscall tracer's tests hold its output to strace's.
          strace
          # Profilers for the `just profile-*` recipes.
          perf
          valgrind
          goStable
          zig
          odin
          gfortran
          ldc2
          nim
          gnatmake
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
        # out of it for their size: samply, hotspot, heaptrack, hyperfine,
        # poop, and KCachegrind.
        profile = default.overrideAttrs (previous: {
          nativeBuildInputs = previous.nativeBuildInputs ++ (with pkgs; [
            samply
            hotspot
            heaptrack
            hyperfine
            poop
            kdePackages.kcachegrind
          ]);
        });
      };

      # The privilege `uscope syscalls` needs for its BPF engine, and
      # `just test-bpf` for its tests: a wrapper around env that raises
      # CAP_BPF and CAP_PERFMON into the ambient set before it runs its
      # command, so they reach uscope through nix develop, systemd-run, and
      # nextest. Only root and the wheel group may run it. A host imports
      # it into its own NixOS or system-manager configuration; neither the
      # dev shell nor the nix store can make a binary with capabilities.
      nixosModules.uscope-bpf = { pkgs, ... }: {
        security.wrappers.uscope-bpf = {
          source = "${pkgs.coreutils}/bin/env";
          owner = "root";
          group = "wheel";
          permissions = "u+rx,g+x";
          capabilities = "cap_bpf,cap_perfmon+ep";
        };
      };
    };
}
