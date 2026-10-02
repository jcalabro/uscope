set shell := ["bash", "-euo", "pipefail", "-c"]
set positional-arguments := true

# Process-isolated debugger tests also create controller, waiter, and inferior
# threads. More concurrency adds contention without improving wall time.
max_test_threads := "16"

# Lints the Rust code and runs the complete test suite.
default: check

# Enters the Nix development shell.
dev *ARGS="":
    exec ./scripts/dev.sh "$@"

# Builds the native test fixtures without running Rust tests.
build-test-programs:
    ./scripts/build-test-programs.sh

# Builds the native test fixtures and uscope.
build: build-test-programs
    cargo build

# Builds uscope and runs it with the supplied arguments.
run *ARGS: build
    ./target/debug/uscope "$@"

# Arguments go to nextest, e.g. `just test print_` or `just test --test cli`.
# Doc tests only run with the full suite.
[doc("Builds the native test fixtures and runs the Rust test suite.")]
test *ARGS: build-test-programs
    test_threads="$(nproc)"; if (( test_threads > {{max_test_threads}} )); then test_threads={{max_test_threads}}; fi; cargo nextest run --test-threads "$test_threads" "$@"
    if (( $# == 0 )); then cargo test --doc; fi

# Runs one fuzz target: value-expression, dwarf-expression, core-dump,
# elf-symbols, disassembly, debug-register-plan, dap-transport, or
# dap-request. Arguments go to libFuzzer.
# iced-x86 builds its formatter tables once and never frees them, which
# LeakSanitizer would report as a failure when the disassembly target exits.
fuzz TARGET *ARGS="":
    if [[ "$1" == disassembly ]]; then set -- "$@" -detect_leaks=0; fi; cargo fuzz run "$1" -- "${@:2}"

# Checks formatting and runs Clippy.
lint:
    cargo fmt --check
    cargo clippy --all-targets --all-features -- -D warnings
    cargo check --quiet --manifest-path fuzz/Cargo.toml

# Checks formatting, runs Clippy, and runs the complete test suite.
check: lint test

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
