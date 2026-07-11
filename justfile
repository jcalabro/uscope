set shell := ["bash", "-euo", "pipefail", "-c"]

# Lints the Rust code and runs the complete test suite.
default: check

# Enters the Nix development shell.
dev *ARGS="":
    exec ./dev.sh {{ARGS}}

# Builds the native test fixtures without running Rust tests.
build-test-programs:
    mkdir -p build/test-programs
    NIX_HARDENING_ENABLE= gcc -std=c17 -Wall -Wextra -Werror -O0 -g3 -fPIE -pie \
        tests/fixtures/basic.c -o build/test-programs/basic

# Builds the native test fixtures and uscope.
build: build-test-programs
    cargo build

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
