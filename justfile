set shell := ["bash", "-euo", "pipefail", "-c"]
set positional-arguments := true

# Process-isolated debugger tests also create controller, waiter, and inferior
# threads. More concurrency adds contention without improving wall time.
max_test_threads := "16"

# Lints the Rust code and runs the complete test suite.
default: check

# Checks formatting, runs Clippy, and runs the complete test suite.
check: lint test

# Runs everything to check before committing.
all: lint test stress sim

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

# Builds the native test fixtures and uscope.
build *ARGS="": build-test-programs
    cargo build {{ARGS}}

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
    test_threads="$(nproc)"; if (( test_threads > {{max_test_threads}} )); then test_threads={{max_test_threads}}; fi; XDG_CONFIG_HOME="$PWD/target/test-config" ./scripts/contained.sh setarch "$(uname -m)" cargo nextest run --test-threads "$test_threads" "$@"
    if (( $# == 0 )); then cargo test --doc; fi

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
    cargo nextest run --no-run
    cpus="$(nproc)"
    burners=()
    trap 'kill "${burners[@]}" 2>/dev/null || true' EXIT
    for (( i = 0; i < cpus / 2; i++ )); do (while :; do :; done) & burners+=($!); done
    XDG_CONFIG_HOME="$PWD/target/test-config" ./scripts/contained.sh setarch "$(uname -m)" cargo nextest run --test-threads "$(( cpus * 2 ))" --stress-count "$1" "${@:2}"

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

# Runs one fuzz target: expression-parse, dwarf-expression, core-dump,
# elf-symbols, gopclntab, disassembly, debug-register-plan, dap-transport,
# dap-request, or views. Arguments go to libFuzzer.
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
