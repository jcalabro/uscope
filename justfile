set shell := ["bash", "-euo", "pipefail", "-c"]
set positional-arguments := true

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
    cargo nextest run --all-targets
    cargo test --doc

# Checks formatting, runs Clippy, and runs the complete test suite.
check: build-test-programs
    cargo fmt --check
    cargo clippy --all-targets --all-features -- -D warnings
    cargo nextest run --all-targets
    cargo test --doc
