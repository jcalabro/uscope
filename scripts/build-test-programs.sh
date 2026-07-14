#!/usr/bin/env bash

set -euo pipefail

readonly output_dir="build/test-programs"
readonly fixtures_dir="tests/fixtures"
readonly c_fixtures_dir="${fixtures_dir}/c"
readonly cpp_fixtures_dir="${fixtures_dir}/cpp"
readonly go_fixtures_dir="${fixtures_dir}/go"
readonly rust_fixtures_dir="${fixtures_dir}/rust"
readonly zig_fixtures_dir="${fixtures_dir}/zig"

source_changed_since_output() {
    local source="$1"
    local output="$2"
    local changed
    changed=$(find "$source" -newer "$output" -print -quit)
    [[ -n "$changed" ]]
}

run_cached_build() {
    local source="$1"
    local output="$2"
    local metadata="$3"
    shift 3
    local -a command=("$@")
    local command_text
    printf -v command_text '%q ' "${command[@]}"
    local signature="${metadata}"$'\n'"command=${command_text}"
    local stamp="${output}.command"
    local previous=""

    if [[ -f "$stamp" ]]; then
        previous=$(<"$stamp")
    fi

    if [[ -x "$output" ]] \
        && ! source_changed_since_output "$source" "$output" \
        && [[ "$previous" == "$signature" ]]; then
        printf '[cached] %s\n' "$output"
        return
    fi

    printf '[build]  %s\n' "$output"
    NIX_HARDENING_ENABLE= "${command[@]}"
    printf '%s\n' "$signature" >"${stamp}.tmp"
    mv "${stamp}.tmp" "$stamp"
}

build_program() {
    local tool="$1"
    local source="$2"
    local output="$3"
    shift 3

    local -a command=(
        "$tool"
        "$@"
        "$source"
        -o "$output"
    )
    local version
    version=$("$tool" --version)
    version=${version%%$'\n'*}
    run_cached_build "$source" "$output" \
        "compiler=${version}"$'\n'"target=x86_64-linux"$'\n'"backend=${tool}" \
        "${command[@]}"
}

build_fixture() {
    local compiler="$1"
    local source="$2"
    local output="$3"
    shift 3
    build_program "$compiler" "$source" "$output" \
        -std=c17 -Wall -Wextra -Werror "$@"
}

build_c_fixture_directory() {
    local compiler="$1"
    local source_dir="$2"
    local output="$3"
    shift 3
    local -a sources=()
    mapfile -d '' sources < <(
        find "$source_dir" -maxdepth 1 -type f -name '*.c' -print0 | sort -z
    )
    if (( ${#sources[@]} == 0 )); then
        printf 'error: C fixture has no source files: %s\n' "$source_dir" >&2
        exit 1
    fi
    local -a command=(
        "$compiler" -std=c17 -Wall -Wextra -Werror "$@" "${sources[@]}" -o "$output"
    )
    local version
    version=$("$compiler" --version)
    version=${version%%$'\n'*}
    run_cached_build "$source_dir" "$output" \
        "compiler=${version}"$'\n'"target=x86_64-linux"$'\n'"backend=${compiler}" \
        "${command[@]}"
}

build_cpp_fixture() {
    local compiler="$1"
    local source="$2"
    local output="$3"
    shift 3
    build_program "$compiler" "$source" "$output" \
        -std=c++20 -Wall -Wextra -Werror "$@"
}

build_shared_fixture() {
    local compiler="$1"
    local source="$2"
    local output="$3"
    shift 3
    build_program "$compiler" "$source" "$output" \
        -std=c17 -Wall -Wextra -Werror -shared -fPIC "$@"
}

build_rust_fixture() {
    local source="$1"
    local output="$2"
    shift 2
    # The no_std fixture uses the system CRT without pulling std's DWARF into the binary.
    build_program rustc "$source" "$output" \
        --edition=2024 -D warnings -C debuginfo=2 -C codegen-units=1 -C panic=abort \
        -C link-arg=-lc "$@"
}

build_go_fixture() {
    local package_dir="$1"
    local output="$2"
    shift 2
    local -a sources=()
    mapfile -d '' sources < <(
        find "$package_dir" -maxdepth 1 -type f -name '*.go' -print0 | sort -z
    )
    if (( ${#sources[@]} == 0 )); then
        printf 'error: Go fixture has no source files: %s\n' "$package_dir" >&2
        exit 1
    fi
    local -a command=(
        env CGO_ENABLED=0 go build -buildvcs=false "$@" -o "$output" "${sources[@]}"
    )
    local version
    version=$(go version)
    local target
    target=$(go env GOOS GOARCH)
    target=${target//$'\n'//}
    run_cached_build "$package_dir" "$output" \
        "compiler=${version}"$'\n'"target=${target}"$'\n'"backend=gc" \
        "${command[@]}"
}

build_zig_fixture() {
    local source="$1"
    local output="$2"
    shift 2
    local -a command=(
        zig build-exe "$source" -target x86_64-linux-gnu -fllvm -fno-strip
        -funwind-tables "$@" "-femit-bin=${output}"
    )
    local version
    version=$(zig version)
    run_cached_build "$source" "$output" \
        "compiler=zig ${version}"$'\n'"target=x86_64-linux-gnu"$'\n'"backend=llvm" \
        "${command[@]}"
}

# Fails the build when a fixture no longer emits a sibling-call jump a test
# depends on, instead of letting the test pass through the regular-callee path.
require_tail_jump() {
    local output="$1"
    local caller="$2"
    local callee="$3"
    # grep reads all input; grep -q would exit early and objdump's SIGPIPE
    # would fail the pipeline under pipefail despite a successful match.
    if ! objdump -d --no-show-raw-insn "$output" \
        | sed -n "/<${caller}>:/,/^\$/p" \
        | grep -E "[[:space:]]jmp[[:space:]]+[^<]*<${callee}>" >/dev/null; then
        printf 'error: %s does not tail-jump from %s to %s\n' \
            "$output" "$caller" "$callee" >&2
        exit 1
    fi
}

# Fails the build when a fixture's DWARF stops exercising the operation a test
# depends on, instead of letting the test pass without its coverage.
require_dwarf_operation() {
    local output="$1"
    local operation="$2"
    # grep reads all input; grep -q would exit early and objdump's SIGPIPE
    # would fail the pipeline under pipefail despite a successful match.
    if ! objdump --dwarf=info,loc "$output" | grep "$operation" >/dev/null; then
        printf 'error: %s does not exercise %s\n' "$output" "$operation" >&2
        exit 1
    fi
}

mkdir -p "$output_dir"

build_fixture gcc "$c_fixtures_dir/basic.c" "$output_dir/basic" \
    -O0 -g3 -fPIE -pie
build_c_fixture_directory gcc "$c_fixtures_dir/pointer-memory" \
    "$output_dir/pointer-memory-gcc-o0" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie
build_fixture gcc "$c_fixtures_dir/variables.c" "$output_dir/variables-gcc-o0" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie
build_fixture clang "$c_fixtures_dir/variables.c" "$output_dir/variables-clang-o0" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie
build_fixture gcc "$c_fixtures_dir/variables.c" "$output_dir/variables-gcc-o2" \
    -O2 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie
require_dwarf_operation "$output_dir/variables-gcc-o2" DW_OP_implicit_pointer
require_dwarf_operation "$output_dir/variables-gcc-o2" 'DW_OP_implicit_pointer:.* 4'
build_fixture clang "$c_fixtures_dir/variables.c" "$output_dir/variables-clang-o2" \
    -O2 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie
build_fixture gcc "$c_fixtures_dir/variables.c" "$output_dir/variables-gcc-nopie" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -fno-pie -no-pie
build_fixture gcc "$c_fixtures_dir/records.c" "$output_dir/records-c-gcc-o0" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie
build_fixture clang "$c_fixtures_dir/records.c" "$output_dir/records-c-clang-o0" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie
build_fixture gcc "$c_fixtures_dir/records.c" "$output_dir/records-c-gcc-o2" \
    -O2 -g3 -gdwarf-5 -fomit-frame-pointer -fPIE -pie
build_fixture clang "$c_fixtures_dir/records.c" "$output_dir/records-c-clang-o2" \
    -O2 -g3 -gdwarf-5 -fomit-frame-pointer -fPIE -pie
build_fixture gcc "$c_fixtures_dir/variables-parameters.c" "$output_dir/variables-parameters-gcc-o0" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie
build_fixture clang "$c_fixtures_dir/variables-parameters.c" "$output_dir/variables-parameters-clang-o0" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie
build_fixture gcc "$c_fixtures_dir/variables-parameters.c" "$output_dir/variables-parameters-gcc-o2" \
    -O2 -g3 -gdwarf-5 -fomit-frame-pointer -fPIE -pie
build_fixture clang "$c_fixtures_dir/variables-parameters.c" "$output_dir/variables-parameters-clang-o2" \
    -O2 -g3 -gdwarf-5 -fomit-frame-pointer -fPIE -pie
build_fixture gcc "$c_fixtures_dir/variables-static.c" "$output_dir/variables-static-gcc-o2" \
    -O2 -g3 -gdwarf-5 -fomit-frame-pointer -fPIE -pie
build_fixture clang "$c_fixtures_dir/variables-static.c" "$output_dir/variables-static-clang-o2" \
    -O2 -g3 -gdwarf-5 -fomit-frame-pointer -fPIE -pie
require_dwarf_operation "$output_dir/variables-static-clang-o2" DW_OP_addrx
build_fixture gcc "$c_fixtures_dir/variables-static.c" "$output_dir/variables-static-gcc-nopie" \
    -O2 -g3 -gdwarf-5 -fomit-frame-pointer -no-pie
require_dwarf_operation "$output_dir/variables-static-gcc-nopie" 'DW_OP_addr:'
build_c_fixture_directory gcc "$c_fixtures_dir/globals" "$output_dir/globals-c-gcc-o0" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie
build_c_fixture_directory clang "$c_fixtures_dir/globals" "$output_dir/globals-c-clang-o0" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie
build_c_fixture_directory gcc "$c_fixtures_dir/globals" "$output_dir/globals-c-gcc-o2" \
    -O2 -g3 -gdwarf-5 -fomit-frame-pointer -fPIE -pie
build_c_fixture_directory clang "$c_fixtures_dir/globals" "$output_dir/globals-c-clang-o2" \
    -O2 -g3 -gdwarf-5 -fomit-frame-pointer -fPIE -pie
build_c_fixture_directory gcc "$c_fixtures_dir/globals" "$output_dir/globals-c-gcc-nopie" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -no-pie
build_shared_fixture gcc "$c_fixtures_dir/shared/library.c" "$output_dir/libglobals.so" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer
build_fixture gcc "$c_fixtures_dir/shared/main.c" "$output_dir/globals-shared" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie -ldl
build_fixture gcc "$c_fixtures_dir/tls.c" "$output_dir/globals-tls-gcc" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie -pthread
build_fixture clang "$c_fixtures_dir/tls.c" "$output_dir/globals-tls-clang" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie -pthread
build_cpp_fixture g++ "$cpp_fixtures_dir/variables.cpp" "$output_dir/variables-cpp-gcc-o0" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie
build_cpp_fixture clang++ "$cpp_fixtures_dir/variables.cpp" "$output_dir/variables-cpp-clang-o0" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie
build_cpp_fixture g++ "$cpp_fixtures_dir/variables.cpp" "$output_dir/variables-cpp-gcc-o2" \
    -O2 -g3 -gdwarf-5 -fomit-frame-pointer -fPIE -pie
build_cpp_fixture clang++ "$cpp_fixtures_dir/variables.cpp" "$output_dir/variables-cpp-clang-o2" \
    -O2 -g3 -gdwarf-5 -fomit-frame-pointer -fPIE -pie
build_cpp_fixture g++ "$cpp_fixtures_dir/records.cpp" "$output_dir/records-cpp-gcc-o0" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie
build_cpp_fixture clang++ "$cpp_fixtures_dir/records.cpp" "$output_dir/records-cpp-clang-o0" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie
build_cpp_fixture g++ "$cpp_fixtures_dir/records.cpp" "$output_dir/records-cpp-gcc-o2" \
    -O2 -g3 -gdwarf-5 -fomit-frame-pointer -fPIE -pie
build_cpp_fixture clang++ "$cpp_fixtures_dir/records.cpp" "$output_dir/records-cpp-clang-o2" \
    -O2 -g3 -gdwarf-5 -fomit-frame-pointer -fPIE -pie
build_cpp_fixture g++ "$cpp_fixtures_dir/globals.cpp" "$output_dir/globals-cpp-gcc-o0" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie
build_cpp_fixture clang++ "$cpp_fixtures_dir/globals.cpp" "$output_dir/globals-cpp-clang-o0" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie
build_cpp_fixture g++ "$cpp_fixtures_dir/globals.cpp" "$output_dir/globals-cpp-gcc-o2" \
    -O2 -g3 -gdwarf-5 -fomit-frame-pointer -fPIE -pie
build_cpp_fixture clang++ "$cpp_fixtures_dir/globals.cpp" "$output_dir/globals-cpp-clang-o2" \
    -O2 -g3 -gdwarf-5 -fomit-frame-pointer -fPIE -pie
build_rust_fixture "$rust_fixtures_dir/variables.rs" "$output_dir/variables-rust-o0" \
    -C opt-level=0 -C force-frame-pointers=yes
build_rust_fixture "$rust_fixtures_dir/variables.rs" "$output_dir/variables-rust-o2" \
    -C opt-level=2 -C force-frame-pointers=no
build_rust_fixture "$rust_fixtures_dir/records.rs" "$output_dir/records-rust-o0" \
    -C opt-level=0 -C force-frame-pointers=yes
build_rust_fixture "$rust_fixtures_dir/records.rs" "$output_dir/records-rust-o2" \
    -C opt-level=2 -C force-frame-pointers=no
build_rust_fixture "$rust_fixtures_dir/globals.rs" "$output_dir/globals-rust-o0" \
    -C opt-level=0 -C force-frame-pointers=yes
build_rust_fixture "$rust_fixtures_dir/globals.rs" "$output_dir/globals-rust-o2" \
    -C opt-level=2 -C force-frame-pointers=no
build_go_fixture "$go_fixtures_dir/variables" "$output_dir/variables-go-o0" \
    -buildmode=pie "-gcflags=all=-N -l"
build_go_fixture "$go_fixtures_dir/variables" "$output_dir/variables-go-o2" \
    -buildmode=pie
build_go_fixture "$go_fixtures_dir/records" "$output_dir/records-go-o0" \
    -buildmode=pie "-gcflags=all=-N -l"
build_go_fixture "$go_fixtures_dir/records" "$output_dir/records-go-o2" \
    -buildmode=pie
build_go_fixture "$go_fixtures_dir/globals" "$output_dir/globals-go-o0" \
    -buildmode=pie "-gcflags=all=-N -l"
build_go_fixture "$go_fixtures_dir/globals" "$output_dir/globals-go-o2" \
    -buildmode=pie
require_dwarf_operation "$output_dir/variables-go-o0" 'DW_AT_language.*Go'
require_dwarf_operation "$output_dir/variables-go-o0" main.inspectScalars
build_zig_fixture "$zig_fixtures_dir/variables.zig" "$output_dir/variables-zig-o0" \
    -O Debug -fPIE -fno-omit-frame-pointer
build_zig_fixture "$zig_fixtures_dir/variables.zig" "$output_dir/variables-zig-o2" \
    -O ReleaseFast -fPIE -fomit-frame-pointer
build_zig_fixture "$zig_fixtures_dir/variables.zig" "$output_dir/variables-zig-nopie" \
    -O Debug -fno-PIE -fno-omit-frame-pointer
build_zig_fixture "$zig_fixtures_dir/records.zig" "$output_dir/records-zig-o0" \
    -O Debug -fPIE -fno-omit-frame-pointer
build_zig_fixture "$zig_fixtures_dir/records.zig" "$output_dir/records-zig-o2" \
    -O ReleaseFast -fPIE -fomit-frame-pointer
build_zig_fixture "$zig_fixtures_dir/records.zig" "$output_dir/records-zig-nopie" \
    -O Debug -fno-PIE -fno-omit-frame-pointer
build_zig_fixture "$zig_fixtures_dir/globals.zig" "$output_dir/globals-zig-o0" \
    -O Debug -fPIE -fno-omit-frame-pointer
build_zig_fixture "$zig_fixtures_dir/globals.zig" "$output_dir/globals-zig-o2" \
    -O ReleaseFast -fPIE -fomit-frame-pointer
build_zig_fixture "$zig_fixtures_dir/globals.zig" "$output_dir/globals-zig-nopie" \
    -O Debug -fno-PIE -fno-omit-frame-pointer
require_dwarf_operation "$output_dir/variables-zig-o0" 'DW_AT_producer.*zig 0.16.0'
require_dwarf_operation "$output_dir/variables-zig-o0" variables.inspectScalars
build_rust_fixture "$rust_fixtures_dir/stepping-boundaries.rs" "$output_dir/stepping-boundaries-rust-o0" \
    -C opt-level=0 -C force-frame-pointers=yes
build_rust_fixture "$rust_fixtures_dir/stepping-boundaries.rs" "$output_dir/stepping-boundaries-rust-o2" \
    -C opt-level=2 -C force-frame-pointers=no
build_zig_fixture "$zig_fixtures_dir/stepping-boundaries.zig" \
    "$output_dir/stepping-boundaries-zig-o0" \
    -O Debug -fPIE -fno-omit-frame-pointer
build_zig_fixture "$zig_fixtures_dir/stepping-boundaries.zig" \
    "$output_dir/stepping-boundaries-zig-o2" \
    -O ReleaseFast -fPIE -fomit-frame-pointer
require_dwarf_operation "$output_dir/stepping-boundaries-zig-o0" \
    'DW_TAG_inlined_subroutine'
build_fixture gcc "$c_fixtures_dir/variables-threads.c" "$output_dir/variables-threads" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie -pthread
build_zig_fixture "$zig_fixtures_dir/variables-threads.zig" \
    "$output_dir/variables-threads-zig" \
    -O Debug -fPIE -fno-omit-frame-pointer
build_fixture gcc "$c_fixtures_dir/variables-inline.c" "$output_dir/variables-inline-gcc-o0" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie
build_fixture clang "$c_fixtures_dir/variables-inline.c" "$output_dir/variables-inline-clang-o0" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie
build_fixture gcc "$c_fixtures_dir/variables-inline.c" "$output_dir/variables-inline-gcc-o1" \
    -O1 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie
build_fixture clang "$c_fixtures_dir/variables-inline.c" "$output_dir/variables-inline-clang-o1" \
    -O1 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie
build_fixture gcc "$c_fixtures_dir/spin.c" "$output_dir/spin" \
    -O0 -g3 -fPIE -pie
build_fixture gcc "$c_fixtures_dir/threads.c" "$output_dir/threads" \
    -O0 -g3 -fPIE -pie -pthread
build_fixture gcc "$c_fixtures_dir/signals.c" "$output_dir/signals" \
    -O0 -g3 -fPIE -pie
build_fixture gcc "$c_fixtures_dir/fatal-signal.c" "$output_dir/fatal-signal" \
    -O0 -g3 -fPIE -pie
build_fixture gcc "$c_fixtures_dir/job-control.c" "$output_dir/job-control" \
    -O0 -g3 -fPIE -pie
build_fixture gcc "$c_fixtures_dir/thread-exec.c" "$output_dir/thread-exec" \
    -O0 -g3 -fPIE -pie -pthread
build_fixture gcc "$c_fixtures_dir/thread-stress.c" "$output_dir/thread-stress" \
    -O0 -g3 -fPIE -pie -pthread
build_fixture gcc "$c_fixtures_dir/step.c" "$output_dir/step" \
    -O0 -g3 -fno-omit-frame-pointer -fPIE -pie
build_fixture gcc "$c_fixtures_dir/stepping-boundaries.c" "$output_dir/stepping-boundaries-gcc-o0" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie
build_fixture clang "$c_fixtures_dir/stepping-boundaries.c" "$output_dir/stepping-boundaries-clang-o0" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie
build_fixture gcc "$c_fixtures_dir/stepping-boundaries.c" "$output_dir/stepping-boundaries-gcc-o2" \
    -O2 -g3 -gdwarf-5 -fomit-frame-pointer -fPIE -pie
build_fixture clang "$c_fixtures_dir/stepping-boundaries.c" "$output_dir/stepping-boundaries-clang-o2" \
    -O2 -g3 -gdwarf-5 -fomit-frame-pointer -mno-red-zone -fPIE -pie
build_fixture gcc "$c_fixtures_dir/tail-calls.c" "$output_dir/tail-calls-gcc-o2" \
    -O2 -g3 -gdwarf-5 -fomit-frame-pointer -fPIE -pie
require_tail_jump "$output_dir/tail-calls-gcc-o2" outer_tail add_one
require_tail_jump "$output_dir/tail-calls-gcc-o2" outer_chain chain_helper
require_tail_jump "$output_dir/tail-calls-gcc-o2" descend_tail mutual_tail
build_fixture clang "$c_fixtures_dir/tail-calls.c" "$output_dir/tail-calls-clang-o2" \
    -O2 -g3 -gdwarf-5 -fomit-frame-pointer -fPIE -pie
require_tail_jump "$output_dir/tail-calls-clang-o2" outer_tail add_one
require_tail_jump "$output_dir/tail-calls-clang-o2" outer_chain chain_helper
require_tail_jump "$output_dir/tail-calls-clang-o2" descend_tail mutual_tail
build_fixture gcc "$c_fixtures_dir/step-over-libc.c" "$output_dir/step-over-libc" \
    -O0 -g3 -fno-omit-frame-pointer -fPIE -pie
build_fixture gcc "$c_fixtures_dir/unwind.c" "$output_dir/unwind-o0" \
    -O0 -g3 -fno-omit-frame-pointer -fPIE -pie
build_fixture gcc "$c_fixtures_dir/unwind.c" "$output_dir/unwind-o2" \
    -O2 -g3 -fomit-frame-pointer -fPIE -pie
build_fixture gcc "$c_fixtures_dir/unwind.c" "$output_dir/unwind-nopie" \
    -O2 -g3 -fomit-frame-pointer -no-pie
build_fixture clang "$c_fixtures_dir/unwind.c" "$output_dir/unwind-clang-o2" \
    -O2 -g3 -fomit-frame-pointer -fPIE -pie
build_fixture gcc "$c_fixtures_dir/inline.c" "$output_dir/inline-gcc-o1" \
    -O1 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie
build_fixture gcc "$c_fixtures_dir/inline.c" "$output_dir/inline-gcc-o2" \
    -O2 -g3 -gdwarf-5 -fomit-frame-pointer -fPIE -pie
build_fixture clang "$c_fixtures_dir/inline.c" "$output_dir/inline-clang-o1" \
    -O1 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie
build_fixture clang "$c_fixtures_dir/inline.c" "$output_dir/inline-clang-o2" \
    -O2 -g3 -gdwarf-5 -fomit-frame-pointer -fPIE -pie
build_fixture gcc "$c_fixtures_dir/inline-threads.c" "$output_dir/inline-threads-gcc-o2" \
    -O2 -g3 -gdwarf-5 -fomit-frame-pointer -fPIE -pie -pthread
build_fixture clang "$c_fixtures_dir/inline-threads.c" "$output_dir/inline-threads-clang-o2" \
    -O2 -g3 -gdwarf-5 -fomit-frame-pointer -fPIE -pie -pthread
