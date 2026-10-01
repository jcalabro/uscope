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

# Fuzzes the bounded structural value-expression parser.
fuzz-value-expression *ARGS="":
    cargo fuzz run value-expression -- "$@"

# Fuzzes bounded DWARF location-expression parsing and evaluation.
fuzz-dwarf-expression *ARGS="":
    cargo fuzz run dwarf-expression -- "$@"

# Fuzzes core-dump parsing, module verification, and post-mortem memory reads.
fuzz-core-dump *ARGS="":
    cargo fuzz run core-dump -- "$@"

# Fuzzes watchpoint debug-register planning and hit attribution invariants.
fuzz-debug-register-plan *ARGS="":
    cargo fuzz run debug-register-plan -- "$@"

# Checks formatting and runs Clippy.
lint:
    cargo fmt --check
    cargo clippy --all-targets --all-features -- -D warnings

# Checks formatting, runs Clippy, and runs the complete test suite.
check: lint test
