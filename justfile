set shell := ["bash", "-euo", "pipefail", "-c"]

default:
    @just --list

build-test-programs:
    mkdir -p build/test-programs
    NIX_HARDENING_ENABLE= gcc -std=c17 -Wall -Wextra -Werror -O0 -g3 -fPIE -pie \
        tests/fixtures/basic.c -o build/test-programs/basic

build: build-test-programs
    cargo build

test: build-test-programs
    cargo test

check: build-test-programs
    cargo fmt --check
    cargo clippy --all-targets --all-features -- -D warnings
    cargo test
