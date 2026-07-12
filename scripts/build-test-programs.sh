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
build_fixture gcc tests/fixtures/variables.c "$output_dir/variables-gcc-o0" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie
build_fixture clang tests/fixtures/variables.c "$output_dir/variables-clang-o0" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie
build_fixture gcc tests/fixtures/variables-threads.c "$output_dir/variables-threads" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie -pthread
build_fixture gcc tests/fixtures/spin.c "$output_dir/spin" \
    -O0 -g3 -fPIE -pie
build_fixture gcc tests/fixtures/threads.c "$output_dir/threads" \
    -O0 -g3 -fPIE -pie -pthread
build_fixture gcc tests/fixtures/signals.c "$output_dir/signals" \
    -O0 -g3 -fPIE -pie
build_fixture gcc tests/fixtures/fatal-signal.c "$output_dir/fatal-signal" \
    -O0 -g3 -fPIE -pie
build_fixture gcc tests/fixtures/job-control.c "$output_dir/job-control" \
    -O0 -g3 -fPIE -pie
build_fixture gcc tests/fixtures/thread-exec.c "$output_dir/thread-exec" \
    -O0 -g3 -fPIE -pie -pthread
build_fixture gcc tests/fixtures/thread-stress.c "$output_dir/thread-stress" \
    -O0 -g3 -fPIE -pie -pthread
build_fixture gcc tests/fixtures/step.c "$output_dir/step" \
    -O0 -g3 -fno-omit-frame-pointer -fPIE -pie
build_fixture gcc tests/fixtures/unwind.c "$output_dir/unwind-o0" \
    -O0 -g3 -fno-omit-frame-pointer -fPIE -pie
build_fixture gcc tests/fixtures/unwind.c "$output_dir/unwind-o2" \
    -O2 -g3 -fomit-frame-pointer -fPIE -pie
build_fixture gcc tests/fixtures/unwind.c "$output_dir/unwind-nopie" \
    -O2 -g3 -fomit-frame-pointer -no-pie
build_fixture clang tests/fixtures/unwind.c "$output_dir/unwind-clang-o2" \
    -O2 -g3 -fomit-frame-pointer -fPIE -pie
build_fixture gcc tests/fixtures/inline.c "$output_dir/inline-gcc-o1" \
    -O1 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie
build_fixture gcc tests/fixtures/inline.c "$output_dir/inline-gcc-o2" \
    -O2 -g3 -gdwarf-5 -fomit-frame-pointer -fPIE -pie
build_fixture clang tests/fixtures/inline.c "$output_dir/inline-clang-o1" \
    -O1 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie
build_fixture clang tests/fixtures/inline.c "$output_dir/inline-clang-o2" \
    -O2 -g3 -gdwarf-5 -fomit-frame-pointer -fPIE -pie
build_fixture gcc tests/fixtures/inline-threads.c "$output_dir/inline-threads-gcc-o2" \
    -O2 -g3 -gdwarf-5 -fomit-frame-pointer -fPIE -pie -pthread
build_fixture clang tests/fixtures/inline-threads.c "$output_dir/inline-threads-clang-o2" \
    -O2 -g3 -gdwarf-5 -fomit-frame-pointer -fPIE -pie -pthread
