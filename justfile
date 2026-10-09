set shell := ["bash", "-euo", "pipefail", "-c"]
set positional-arguments := true

# Process-isolated debugger tests also create controller, waiter, and inferior
# threads. More concurrency adds contention without improving wall time.
max_test_threads := "16"

# Lints the Rust code and runs the complete test suite.
default: check

# Checks formatting, runs Clippy, and runs the complete test suite.
check: lint test web-test

# Runs everything to check before committing.
all: check web-e2e stress sim bench-smoke

# Enters the Nix development shell.
dev *ARGS="":
    exec ./scripts/dev.sh "$@"

# Installs the vscode extension as a symlink for fast local development.
install-vscode-symlink:
    ln -s "$PWD/editors/vscode" ~/.vscode/extensions/uscope.uscope-0.1.0

# Builds the native test fixtures and the simulator's golden programs
# without running Rust tests.
build-test-programs: golden
    ./scripts/build-test-programs.sh

# Builds the simulator's golden programs into build/golden, failing unless
# they match the hashes and behavior their manifests record.
golden:
    ./scripts/golden.sh build

# Builds the native test fixtures and uscope. uscope builds with the test
# profile, as the tests do, whose optimization loads debug information five
# times as fast as an unoptimized build, and which shares the tests' artifacts.
build *ARGS="": build-test-programs
    cargo build --profile test {{ARGS}}

# Builds uscope and runs it with the supplied arguments.
run *ARGS: build
    ./target/debug/uscope "$@"

# Checks formatting and runs Clippy on development and release builds, which
# differ in what the flight recorder compiles. Incremental checking halves the
# release lint after an edit and leaves release builds as they are.
lint:
    cargo fmt --check
    cargo clippy --all-targets --all-features -- -D warnings
    CARGO_PROFILE_RELEASE_INCREMENTAL=true cargo clippy --release --all-targets --all-features -- -D warnings
    cargo check --quiet --manifest-path fuzz/Cargo.toml

# Arguments go to nextest, e.g. `just test print_` or `just test --test cli`.
# Doc tests only run with the full suite. Tests run inside a memory cap, and
# each test process also caps its own heap (tests/support/memory_cap.rs). `nix develop` turns address
# randomization off, which setarch turns back on, so tests see what they would
# in any shell. A user's own view files are not the tests', so the user
# configuration is an empty directory.
[doc("Builds the native test fixtures and runs the Rust test suite.")]
test *ARGS: build-test-programs
    test_threads="$(nproc)"; if (( test_threads > {{max_test_threads}} )); then test_threads={{max_test_threads}}; fi; XDG_CONFIG_HOME="$PWD/target/test-config" ./scripts/contained.sh setarch "$(uname -m)" cargo nextest run --features tools --test-threads "$test_threads" "$@"
    if (( $# == 0 )); then cargo test --doc; fi

# Builds the tokio fixtures where their sources changed and runs the tokio
# suite: the quick loop for work on tokio support. Arguments go to nextest,
# e.g. `just tokio workers::`.
tokio *ARGS:
    ./scripts/build-test-programs.sh tokio
    test_threads="$(nproc)"; if (( test_threads > {{max_test_threads}} )); then test_threads={{max_test_threads}}; fi; XDG_CONFIG_HOME="$PWD/target/test-config" ./scripts/contained.sh setarch "$(uname -m)" cargo nextest run --features tools --test-threads "$test_threads" --test tokio "$@"

# Races in process control fail far more often when the debugger competes for
# the CPUs, so this oversubscribes the test threads and keeps busy loops
# running beside them. It stops at the first failure so that the failing
# test's flight recording is kept; a later pass of the same test would remove
# it. Arguments go to nextest, e.g. `just stress 100 -E 'binary(dap)'`.
[doc("Runs the test suite COUNT times under CPU load.")]
stress COUNT="10" *ARGS: build-test-programs
    #!/usr/bin/env bash
    set -euo pipefail
    # Build before the busy loops start so they slow only the tests.
    cargo nextest run --features tools --no-run
    cpus="$(nproc)"
    burners=()
    trap 'kill "${burners[@]}" 2>/dev/null || true' EXIT
    for (( i = 0; i < cpus / 2; i++ )); do (while :; do :; done) & burners+=($!); done
    XDG_CONFIG_HOME="$PWD/target/test-config" ./scripts/contained.sh setarch "$(uname -m)" cargo nextest run --features tools --test-threads "$(( cpus * 2 ))" --stress-count "$1" "${@:2}"

# Attaches uscope to a tokio server under load for MINUTES, inspecting and
# re-attaching it over and over, with every invariant checked at every stop.
soak MINUTES="10": build-test-programs
    #!/usr/bin/env bash
    set -euo pipefail
    cargo nextest run --features tools --no-run
    USCOPE_SOAK_SECONDS="$(( $1 * 60 ))" XDG_CONFIG_HOME="$PWD/target/test-config" ./scripts/contained.sh setarch "$(uname -m)" cargo nextest run --features tools --profile soak --no-capture --run-ignored only --test tokio -E 'test(=soak::soak)'

# Installs the web page's locked dependencies.
web-deps:
    cd web && pnpm install --frozen-lockfile --silent

# Builds the web page into build/web, where `uscope web` serves it from.
# Development builds of uscope read it from disk, so rebuilding the page
# needs no Rust rebuild.
web: web-deps
    cd web && ./node_modules/.bin/vite build --logLevel warn

# Type-checks and lints the page and runs its tests outside a browser and
# its component tests in headless Chromium. Arguments go to Vitest.
web-test *ARGS: web-deps
    cd web && ./node_modules/.bin/tsc --noEmit && biome check src test e2e && ./node_modules/.bin/vitest run "$@"

# Drives the built page and real `uscope web` servers in Chromium and
# Firefox. Arguments go to Playwright, e.g. `just web-e2e --project=chromium`.
web-e2e *ARGS: web build-test-programs
    cargo build --quiet --profile test
    cd web && ./node_modules/.bin/playwright test "$@"

# Serves PROGRAM on port 7342 with the page from Vite, which reloads on every
# save: open the join link uscope prints, with 5173 in place of 7342.
web-dev *ARGS: web-deps
    #!/usr/bin/env bash
    set -euo pipefail
    cargo build --quiet --profile test
    ./target/debug/uscope web --port 7342 --allow-origin http://127.0.0.1:5173 "$@" &
    trap 'kill %1 2>/dev/null || true' EXIT
    cd web && ./node_modules/.bin/vite

# Rerecords the server traffic the page's replay tests read
# (web/test/transcripts) from the Rust web tests.
web-transcripts: build-test-programs
    USCOPE_WEB_TRANSCRIPTS="$PWD/web/test/transcripts" cargo nextest run --features tools --test web

# Rebuilds one golden program and rewrites its manifest, after a deliberate
# change to its sources or the toolchain. Commit a new manifest on its own.
golden-record NAME:
    ./scripts/golden.sh record "$1"

# Simulates random sessions on every core for SECONDS, inside a memory cap,
# and reports each kind of failure with its smallest seed.
sim SECONDS="30": golden
    cargo build --profile sim --features sim --bin uscope-sim
    ./scripts/contained.sh ./target/sim/uscope-sim sweep --seconds "$1"

# Replays one simulated session and prints its trace. Pass `--at STEP` to
# stop there and print the state, or the `--fingerprint` a report gave.
sim-seed SEED *ARGS: golden
    cargo build --profile sim --features sim --bin uscope-sim
    ./target/sim/uscope-sim replay "$1" "${@:2}"

# Summarizes a `--timings` report, or what changed between two:
# `just timings BASE NEW`. Pass --threads for each thread's work, and --all
# for every phase.
timings *ARGS:
    cargo run --quiet --profile test --features tools --bin uscope-tools -- timings "$@"

# Prints every answer PROGRAM's debug information gives, in a canonical form
# that compares with `diff`. Pass --sections to dump only some.
dump PROGRAM *ARGS:
    cargo run --quiet --profile test --features tools --bin uscope-tools -- dump "$@"

# Loads every program of the pinned corpus in processes of their own and
# reports what each load costs. Pass --out FILE to save the report and
# --compare BASE to compare it with a saved one; `just bench-large` builds
# the large program first.
bench *ARGS: build-test-programs
    cargo build --quiet --release --features tools --bin uscope-tools
    ./scripts/contained.sh ./target/release/uscope-tools bench "$@"

# The seconds-long benchmark `just all` runs: it fails when loading the
# small programs allocates more than bench/baseline.json records. Record a
# deliberate change with `just bench-smoke --record bench/baseline.json`.
bench-smoke *ARGS: build-test-programs
    cargo build --quiet --profile test --features tools --bin uscope-tools
    ./scripts/contained.sh ./target/debug/uscope-tools bench --corpus smoke --repeat 1 --check bench/baseline.json "$@"

# Builds the full benchmark's large program: uscope's own development build
# at a pinned commit, in a worktree, with its paths remapped so that its
# bytes do not depend on the checkout. Fails unless they match the digest.
large_commit := "d089093868eba621d618c059b9bb7374dd61c9fe"
large_digest := "db849566d2cc0547bdb9c209da44075a9b6dd1b14b45b1814cadfce8ae44e480"
bench-large:
    #!/usr/bin/env bash
    set -euo pipefail
    dir="$PWD/target/bench/large"
    [[ -d "$dir/src" ]] || git worktree add --quiet --detach "$dir/src" {{large_commit}}
    git -C "$dir/src" checkout --quiet --detach {{large_commit}}
    (cd "$dir/src" && RUSTFLAGS="$RUSTFLAGS --remap-path-prefix=$dir/src=/uscope" CARGO_TARGET_DIR="$dir/target" cargo build --quiet --bin uscope)
    cp "$dir/target/debug/uscope" "$dir/uscope"
    digest="$(sha256sum "$dir/uscope" | cut -d' ' -f1)"
    if [[ -n "{{large_digest}}" && "$digest" != "{{large_digest}}" ]]; then
        echo "error: the large program's digest is $digest, not {{large_digest}}" >&2
        exit 1
    fi
    echo "$dir/uscope $digest"

# Prints where loading PROGRAM spends its instructions: Callgrind's
# inclusive costs, trimmed to uscope's functions.
profile-instructions PROGRAM:
    #!/usr/bin/env bash
    set -euo pipefail
    cargo build --quiet --profile profiling --features tools --bin uscope-tools
    out="$(mktemp)"
    trap 'rm -f "$out"' EXIT
    valgrind --tool=callgrind --callgrind-out-file="$out" ./target/profiling/uscope-tools load "$1" >/dev/null 2>&1
    callgrind_annotate --inclusive=yes "$out" | grep -E 'PROGRAM TOTALS|uscope' | head -80

# Prints which uscope functions allocate when loading PROGRAM, by blocks
# and by bytes live at the heap's peak, under DHAT with the C library's
# allocator, which Valgrind can see.
profile-heap PROGRAM:
    #!/usr/bin/env bash
    set -euo pipefail
    cargo build --quiet --profile profiling --features tools,system-alloc --bin uscope-tools
    out="$(mktemp)"
    trap 'rm -f "$out"' EXIT
    valgrind --tool=dhat --dhat-out-file="$out" ./target/profiling/uscope-tools load "$1" >/dev/null 2>&1
    ./target/profiling/uscope-tools heap "$out"

# Counts user-mode instructions, cycles, and cache and branch misses while
# loading PROGRAM.
profile-counters PROGRAM:
    cargo build --quiet --profile profiling --features tools --bin uscope-tools
    perf stat -e instructions:u,cycles:u,cache-references:u,cache-misses:u,branch-misses:u ./target/profiling/uscope-tools load "$1" >/dev/null

# Runs one fuzz target: expression-parse, dwarf-expression, core-dump, dispatch,
# elf-symbols, gopclntab, disassembly, debug-register-plan, dap-transport,
# dap-request, views, or image. Arguments go to libFuzzer.
# iced-x86 builds its formatter tables once and never frees them, which
# LeakSanitizer would report as a failure when the disassembly target exits.
fuzz TARGET *ARGS="":
    if [[ "$1" == disassembly ]]; then set -- "$@" -detect_leaks=0; fi; cargo fuzz run "$1" -- "${@:2}"

# Drives the DAP adapter from a real VS Code window, as a user would, and
# records each session's traffic in DIR. Needs a display. Recording into
# tests/dap/traffic refreshes the traffic the DAP tests replay.
uat-vscode DIR="target/uat": build
    PATH="$PWD/target/debug:$PATH" editors/vscode/test/run.sh "$1"
    sed -i "s#$PWD#\${root}#g" "$1"/vscode-*.log

# Drives the DAP adapter from nvim-dap in a headless Neovim and records each
# session's traffic in DIR. NVIM_DAP is an nvim-dap checkout.
uat-nvim NVIM_DAP DIR="target/uat": build
    rm -f "$2"/nvim-*.log
    PATH="$PWD/target/debug:$PATH" nvim --headless --clean -l editors/nvim/uat.lua "$1" "$PWD" "$2"
    sed -i "s#$PWD#\${root}#g" "$2"/nvim-*.log

# Screenshots every screen in light and dark at three widths into
# target/web-shots, with PROGRAM loaded (the kvstore fixture by default).
web-shot *PROGRAM: web build-test-programs
    cargo build --quiet --profile test
    cd web && node e2e/shots.ts "$@"

# Drives PROGRAM through STEPS in headless Chromium, saving a screenshot after
# each and printing the page's console: `just web-probe build/test-programs/basic
# key:F9 key:F5 wait:Stopped`. See web/e2e/probe.ts for the steps.
web-probe PROGRAM *STEPS: web
    cargo build --quiet --profile test
    cd web && node e2e/probe.ts "$(realpath "../$1")" "${@:2}"
