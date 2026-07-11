#!/usr/bin/env bash

set -euo pipefail

readonly output_dir="build/test-programs"

build_fixture() {
    local compiler="$1"
    local source="$2"
    local output="$3"
    shift 3

    local -a command=(
        "$compiler"
        -std=c17
        -Wall
        -Wextra
        -Werror
        "$@"
        "$source"
        -o "$output"
    )
    local version
    version=$("$compiler" --version)
    version=${version%%$'\n'*}

    local command_text
    printf -v command_text '%q ' "${command[@]}"
    local signature="compiler=${version}"$'\n'"command=${command_text}"
    local stamp="${output}.command"
    local previous=""

    if [[ -f "$stamp" ]]; then
        previous=$(<"$stamp")
    fi

    if [[ -x "$output" && ! "$source" -nt "$output" && "$previous" == "$signature" ]]; then
        printf '[cached] %s\n' "$output"
        return
    fi

    printf '[build]  %s\n' "$output"
    NIX_HARDENING_ENABLE= "${command[@]}"
    printf '%s\n' "$signature" >"${stamp}.tmp"
    mv "${stamp}.tmp" "$stamp"
}

mkdir -p "$output_dir"

build_fixture gcc tests/fixtures/basic.c "$output_dir/basic" \
    -O0 -g3 -fPIE -pie
build_fixture gcc tests/fixtures/spin.c "$output_dir/spin" \
    -O0 -g3 -fPIE -pie
build_fixture gcc tests/fixtures/unwind.c "$output_dir/unwind-o0" \
    -O0 -g3 -fno-omit-frame-pointer -fPIE -pie
build_fixture gcc tests/fixtures/unwind.c "$output_dir/unwind-o2" \
    -O2 -g3 -fomit-frame-pointer -fPIE -pie
build_fixture gcc tests/fixtures/unwind.c "$output_dir/unwind-nopie" \
    -O2 -g3 -fomit-frame-pointer -no-pie
build_fixture clang tests/fixtures/unwind.c "$output_dir/unwind-clang-o2" \
    -O2 -g3 -fomit-frame-pointer -fPIE -pie
