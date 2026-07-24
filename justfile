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
run *ARGS="": build
    ./target/debug/uscope "$@"

# Builds the native test fixtures and runs the Rust test suite.
test: build-test-programs
    test_threads="$(nproc)"; if (( test_threads > {{max_test_threads}} )); then test_threads={{max_test_threads}}; fi; cargo nextest run --all-targets --test-threads "$test_threads"
    cargo test --doc

# Fuzzes the bounded structural value-expression parser.
fuzz-value-expression *ARGS="":
    cargo fuzz run value-expression -- "$@"

# Checks formatting, runs Clippy, and runs the complete test suite.
check: build-test-programs
    cargo fmt --check
    cargo clippy --all-targets --all-features -- -D warnings
    test_threads="$(nproc)"; if (( test_threads > {{max_test_threads}} )); then test_threads={{max_test_threads}}; fi; cargo nextest run --all-targets --test-threads "$test_threads"
    cargo test --doc
