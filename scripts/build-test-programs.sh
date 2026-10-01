#!/usr/bin/env bash

set -euo pipefail

readonly output_dir="build/test-programs"
readonly fixtures_dir="tests/fixtures"
readonly c_fixtures_dir="${fixtures_dir}/c"
readonly cpp_fixtures_dir="${fixtures_dir}/cpp"
readonly go_fixtures_dir="${fixtures_dir}/go"
readonly rust_fixtures_dir="${fixtures_dir}/rust"
readonly zig_fixtures_dir="${fixtures_dir}/zig"
readonly suite_stamp="${output_dir}/.suite.stamp"
readonly suite_outputs="${output_dir}/.suite.outputs"

declare -A dash_version_by_tool=()
declare -A rebuilt_outputs=()
dash_version=""
gdb_version=""
go_version=""
go_target=""
zig_version=""

read_dash_version() {
    local tool="$1"
    if [[ ! -v "dash_version_by_tool[$tool]" ]]; then
        local version
        version=$("$tool" --version)
        dash_version_by_tool["$tool"]=${version%%$'\n'*}
    fi
    dash_version=${dash_version_by_tool["$tool"]}
}

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
        rebuilt_outputs["$output"]=false
        printf '[cached] %s\n' "$output"
        return
    fi

    printf '[build]  %s\n' "$output"
    NIX_HARDENING_ENABLE= "${command[@]}"
    rebuilt_outputs["$output"]=true
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
    read_dash_version "$tool"
    run_cached_build "$source" "$output" \
        "compiler=${dash_version}"$'\n'"target=x86_64-linux"$'\n'"backend=${tool}" \
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
    read_dash_version "$compiler"
    run_cached_build "$source_dir" "$output" \
        "compiler=${dash_version}"$'\n'"target=x86_64-linux"$'\n'"backend=${compiler}" \
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
    if [[ -z "$go_version" ]]; then
        go_version=$(go version)
        go_target=$(go env GOOS GOARCH)
        go_target=${go_target//$'\n'//}
    fi
    run_cached_build "$package_dir" "$output" \
        "compiler=${go_version}"$'\n'"target=${go_target}"$'\n'"backend=gc" \
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
    if [[ -z "$zig_version" ]]; then
        zig_version=$(zig version)
    fi
    run_cached_build "$source" "$output" \
        "compiler=zig ${zig_version}"$'\n'"target=x86_64-linux-gnu"$'\n'"backend=llvm" \
        "${command[@]}"
}

validation_is_cached() {
    local output="$1"
    local stamp="$2"
    local signature="$3"
    local previous=""

    if [[ -f "$stamp" ]]; then
        previous=$(<"$stamp")
    fi
    [[ "${rebuilt_outputs[$output]:-true}" == false && "$previous" == "$signature" ]]
}

record_validation() {
    local stamp="$1"
    local signature="$2"
    printf '%s\n' "$signature" >"${stamp}.tmp"
    mv "${stamp}.tmp" "$stamp"
}

# Fails the build when a fixture no longer emits a sibling-call jump a test
# depends on, instead of letting the test pass through the regular-callee path.
require_tail_jump() {
    local output="$1"
    local caller="$2"
    local callee="$3"
    local stamp="${output}.validation-tail-${caller}-${callee}"
    local signature="validator=tail-jump-v1"$'\n'"caller=${caller}"$'\n'"callee=${callee}"
    if validation_is_cached "$output" "$stamp" "$signature"; then
        return
    fi
    # grep reads all input; grep -q would exit early and objdump's SIGPIPE
    # would fail the pipeline under pipefail despite a successful match.
    if ! objdump -d --no-show-raw-insn "$output" \
        | sed -n "/<${caller}>:/,/^\$/p" \
        | grep -E "[[:space:]]jmp[[:space:]]+[^<]*<${callee}>" >/dev/null; then
        printf 'error: %s does not tail-jump from %s to %s\n' \
            "$output" "$caller" "$callee" >&2
        exit 1
    fi
    record_validation "$stamp" "$signature"
}

# Fails the build when a fixture's DWARF stops exercising the operation a test
# depends on, instead of letting the test pass without its coverage.
require_dwarf_operation() {
    local output="$1"
    local operation="$2"
    local key=${operation//[^a-zA-Z0-9]/_}
    local stamp="${output}.validation-dwarf-${key}"
    local signature="validator=dwarf-operation-v1"$'\n'"operation=${operation}"
    if validation_is_cached "$output" "$stamp" "$signature"; then
        return
    fi
    # grep reads all input; grep -q would exit early and objdump's SIGPIPE
    # would fail the pipeline under pipefail despite a successful match.
    if ! objdump --dwarf=info,loc "$output" | grep "$operation" >/dev/null; then
        printf 'error: %s does not exercise %s\n' "$output" "$operation" >&2
        exit 1
    fi
    record_validation "$stamp" "$signature"
}

# Records a post-mortem core of a fixture with gdb's gcore. The fixture must
# stop with the expected signal first, so a fixture that stops crashing fails
# the build instead of silently producing a different core. FILTER becomes the
# inferior's coredump_filter, which gcore honors like the kernel.
core_signature() {
    local signal="$1"
    local filter="$2"
    local inputs="$3"
    shift 3
    if [[ -z "$gdb_version" ]]; then
        gdb_version=$(gdb --version)
        gdb_version=${gdb_version%%$'\n'*}
    fi
    local -a input_paths
    read -r -a input_paths <<<"$inputs"
    local command_text
    printf -v command_text '%q ' "$@"
    printf 'generator=gcore-v2\ngdb=%s\nsignal=%s\nfilter=%s\ncommand=%s\n' \
        "$gdb_version" "$signal" "$filter" "$command_text"
    stat -L --format='%n %Y %s' "${input_paths[@]}"
}

core_is_current() {
    local core="$1"
    local signature="$2"
    [[ -s "$core" && -f "${core}.command" && "$(<"${core}.command")" == "$signature" ]]
}

generate_core() {
    local core="$1"
    local signal="$2"
    local filter="$3"
    local inputs="$4"
    shift 4
    local signature
    signature=$(core_signature "$signal" "$filter" "$inputs" "$@")
    local stamp="${core}.command"
    if core_is_current "$core" "$signature"; then
        rebuilt_outputs["$core"]=false
        printf '[cached] %s\n' "$core"
        return
    fi

    printf '[core]   %s\n' "$core"
    local temporary="${core}.tmp"
    rm -f "$temporary"
    local log
    # Randomized load addresses make every relocation path in the reader do
    # real work. gdb only stops disabling randomization itself, so setarch also
    # clears any ADDR_NO_RANDOMIZE personality inherited from the caller.
    log=$(bash -c 'printf "%s\n" "$1" >/proc/self/coredump_filter && shift && exec "$@"' \
        _ "$filter" \
        setarch "$(uname -m)" \
        gdb -nx -batch -q \
        -iex 'set auto-load off' \
        -iex 'set debuginfod enabled off' \
        -ex 'set disable-randomization off' \
        -ex 'set pagination off' \
        -ex 'set confirm off' \
        -ex 'run' \
        -ex 'printf "uscope-signal=%d\n", $_siginfo.si_signo' \
        -ex "gcore ${temporary}" \
        -ex 'kill' \
        --args "$@" 2>&1) || true
    if ! grep -Fx "uscope-signal=${signal}" <<<"$log" >/dev/null; then
        printf 'error: %s did not stop with signal %s for a core dump:\n%s\n' \
            "$1" "$signal" "$log" >&2
        rm -f "$temporary"
        exit 1
    fi
    # gdb's batch status reflects only its last command, so gcore's own
    # completion message is the evidence that the core was fully written.
    if ! grep -Fx "Saved corefile ${temporary}" <<<"$log" >/dev/null || [[ ! -s "$temporary" ]]; then
        printf 'error: gcore did not write %s:\n%s\n' "$temporary" "$log" >&2
        rm -f "$temporary"
        exit 1
    fi
    mv "$temporary" "$core"
    rebuilt_outputs["$core"]=true
    printf '%s\n' "$signature" >"${stamp}.tmp"
    mv "${stamp}.tmp" "$stamp"
}

# Nix store paths identify each toolchain, so their resolved paths and mtimes
# change with compiler versions without spawning version probes.
suite_signature() {
    local -a paths=()
    local tool path
    for tool in gcc g++ clang clang++ rustc go zig objdump gdb setarch; do
        if path=$(type -P "$tool"); then
            paths+=("$path")
        fi
    done
    printf 'suite-v1\nGOOS=%s GOARCH=%s\n' "${GOOS-}" "${GOARCH-}"
    stat -L --format='%n %Y' "${paths[@]}"
}

# Skips every per-fixture probe when no fixture source, this script, or tool
# changed and every previously built output still exists.
suite_is_current() {
    local signature="$1"
    [[ -f "$suite_stamp" && -f "$suite_outputs" ]] || return 1
    [[ "$(<"$suite_stamp")" == "$signature" ]] || return 1
    [[ -z "$(find "$fixtures_dir" "${BASH_SOURCE[0]}" -newer "$suite_stamp" -print -quit)" ]] \
        || return 1
    local output
    while IFS= read -r output; do
        [[ -e "$output" ]] || return 1
    done <"$suite_outputs"
}

mkdir -p "$output_dir"
signature=$(suite_signature)
if suite_is_current "$signature"; then
    printf '[cached] %s\n' "$output_dir"
    exit 0
fi
# Written before building so sources edited during this run are newer than the
# stamp that the final rename publishes.
printf '%s\n' "$signature" >"${suite_stamp}.tmp"

build_fixture gcc "$c_fixtures_dir/basic.c" "$output_dir/basic" \
    -O0 -g3 -fPIE -pie
build_fixture gcc "$c_fixtures_dir/attach.c" "$output_dir/attach" \
    -O0 -g3 -fPIE -pie
build_fixture gcc "$c_fixtures_dir/attach-threads.c" "$output_dir/attach-threads" \
    -O0 -g3 -fPIE -pie -pthread
build_c_fixture_directory gcc "$c_fixtures_dir/pointer-memory" \
    "$output_dir/pointer-memory-gcc-o0" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie
build_c_fixture_directory clang "$c_fixtures_dir/pointer-memory" \
    "$output_dir/pointer-memory-clang-o0" \
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
build_fixture gcc "$c_fixtures_dir/enums.c" "$output_dir/enums-c-gcc-o0" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie
build_fixture clang "$c_fixtures_dir/enums.c" "$output_dir/enums-c-clang-o0" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie
build_fixture gcc "$c_fixtures_dir/enums.c" "$output_dir/enums-c-gcc-o2" \
    -O2 -g3 -gdwarf-5 -fomit-frame-pointer -fPIE -pie
build_fixture clang "$c_fixtures_dir/enums.c" "$output_dir/enums-c-clang-o2" \
    -O2 -g3 -gdwarf-5 -fomit-frame-pointer -fPIE -pie
build_fixture gcc "$c_fixtures_dir/types.c" "$output_dir/types-c-gcc-o0" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie
build_fixture clang "$c_fixtures_dir/types.c" "$output_dir/types-c-clang-o0" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie
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
build_shared_fixture gcc "$c_fixtures_dir/module-frames/library.c" "$output_dir/libmodule-frames.so" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer
build_fixture gcc "$c_fixtures_dir/module-frames/main.c" "$output_dir/module-frames-gcc-o0" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie \
    "-L$output_dir" -lmodule-frames '-Wl,-rpath,$ORIGIN'
build_fixture clang "$c_fixtures_dir/module-frames/main.c" "$output_dir/module-frames-clang-o2" \
    -O2 -g3 -gdwarf-5 -fomit-frame-pointer -fPIE -pie \
    "-L$output_dir" -lmodule-frames '-Wl,-rpath,$ORIGIN'
build_fixture gcc "$c_fixtures_dir/module-frames/main.c" "$output_dir/module-frames-gcc-nopie" \
    -O2 -g3 -gdwarf-5 -fomit-frame-pointer -no-pie \
    "-L$output_dir" -lmodule-frames '-Wl,-rpath,$ORIGIN'
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
build_cpp_fixture g++ "$cpp_fixtures_dir/enums.cpp" "$output_dir/enums-cpp-gcc-o0" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie
build_cpp_fixture clang++ "$cpp_fixtures_dir/enums.cpp" "$output_dir/enums-cpp-clang-o0" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie
build_cpp_fixture g++ "$cpp_fixtures_dir/enums.cpp" "$output_dir/enums-cpp-gcc-o2" \
    -O2 -g3 -gdwarf-5 -fomit-frame-pointer -fPIE -pie
build_cpp_fixture clang++ "$cpp_fixtures_dir/enums.cpp" "$output_dir/enums-cpp-clang-o2" \
    -O2 -g3 -gdwarf-5 -fomit-frame-pointer -fPIE -pie
build_cpp_fixture g++ "$cpp_fixtures_dir/types.cpp" "$output_dir/types-cpp-gcc-dwarf4" \
    -O0 -g3 -gdwarf-4 -fdebug-types-section -fno-omit-frame-pointer -fPIE -pie
build_cpp_fixture g++ "$cpp_fixtures_dir/types.cpp" "$output_dir/types-cpp-gcc-dwarf5" \
    -O0 -g3 -gdwarf-5 -fdebug-types-section -fno-omit-frame-pointer -fPIE -pie
require_dwarf_operation "$output_dir/types-cpp-gcc-dwarf4" 'DW_AT_type.*signature:'
require_dwarf_operation "$output_dir/types-cpp-gcc-dwarf5" 'DW_AT_type.*signature:'
build_rust_fixture "$rust_fixtures_dir/variables.rs" "$output_dir/variables-rust-o0" \
    -C opt-level=0 -C force-frame-pointers=yes
build_rust_fixture "$rust_fixtures_dir/variables.rs" "$output_dir/variables-rust-o2" \
    -C opt-level=2 -C force-frame-pointers=no
build_rust_fixture "$rust_fixtures_dir/records.rs" "$output_dir/records-rust-o0" \
    -C opt-level=0 -C force-frame-pointers=yes
build_rust_fixture "$rust_fixtures_dir/records.rs" "$output_dir/records-rust-o2" \
    -C opt-level=2 -C force-frame-pointers=no
build_rust_fixture "$rust_fixtures_dir/enums.rs" "$output_dir/enums-rust-o0" \
    -C opt-level=0 -C force-frame-pointers=yes
build_rust_fixture "$rust_fixtures_dir/enums.rs" "$output_dir/enums-rust-o2" \
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
build_go_fixture "$go_fixtures_dir/enums" "$output_dir/enums-go-o0" \
    -buildmode=pie "-gcflags=all=-N -l"
build_go_fixture "$go_fixtures_dir/enums" "$output_dir/enums-go-o2" \
    -buildmode=pie
require_dwarf_operation "$output_dir/variables-go-o0" 'DW_AT_language.*Go'
require_dwarf_operation "$output_dir/variables-go-o0" main.inspectScalars
require_dwarf_operation "$output_dir/enums-go-o0" 'DW_TAG_constant'
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
build_zig_fixture "$zig_fixtures_dir/enums.zig" "$output_dir/enums-zig-o0" \
    -O Debug -fPIE -fno-omit-frame-pointer
build_zig_fixture "$zig_fixtures_dir/enums.zig" "$output_dir/enums-zig-o2" \
    -O ReleaseFast -fPIE -fomit-frame-pointer
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

build_shared_fixture gcc "$c_fixtures_dir/crash/library.c" "$output_dir/libcrash.so" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -Wl,--build-id
build_fixture gcc "$c_fixtures_dir/crash/main.c" "$output_dir/crash-gcc-o0" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie -pthread -Wl,--build-id \
    "-L$output_dir" -lcrash '-Wl,-rpath,$ORIGIN'
build_fixture gcc "$c_fixtures_dir/crash/main.c" "$output_dir/crash-gcc-o0-rebuilt" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie -pthread -Wl,--build-id \
    -DCRASH_REBUILT "-L$output_dir" -lcrash '-Wl,-rpath,$ORIGIN'
build_fixture clang "$c_fixtures_dir/crash/main.c" "$output_dir/crash-clang-o2" \
    -O2 -g3 -gdwarf-5 -fomit-frame-pointer -fPIE -pie -pthread \
    "-L$output_dir" -lcrash '-Wl,-rpath,$ORIGIN'
require_dwarf_operation "$output_dir/crash-clang-o2" 'DW_OP_reg17 (xmm0)'
build_fixture gcc "$c_fixtures_dir/crash/main.c" "$output_dir/crash-gcc-o2-nopie" \
    -O2 -g3 -gdwarf-5 -fomit-frame-pointer -no-pie -pthread \
    "-L$output_dir" -lcrash '-Wl,-rpath,$ORIGIN'
build_rust_fixture "$rust_fixtures_dir/crash.rs" "$output_dir/crash-rust-o0" \
    -C opt-level=0 -C force-frame-pointers=yes
build_go_fixture "$go_fixtures_dir/crash" "$output_dir/crash-go-o0" \
    -buildmode=pie "-gcflags=all=-N -l"
build_zig_fixture "$zig_fixtures_dir/crash.zig" "$output_dir/crash-zig-o0" \
    -O Debug -fPIE -fno-omit-frame-pointer

# Post-mortem cores. 0x33 is the kernel's default coredump_filter; 0x23 omits
# ELF header pages, 0x10 saves only ELF header pages so modified file-backed
# pages are omitted too, and 0 saves no memory at all, leaving nothing that can
# verify a module file.
readonly default_core_filter=0x33
readonly headerless_core_filter=0x23
readonly headers_only_core_filter=0x10
readonly memoryless_core_filter=0x0
for variant in gcc-o0 clang-o2 gcc-o2-nopie; do
    program="$output_dir/crash-${variant}"
    inputs="$program $output_dir/libcrash.so"
    generate_core "$output_dir/crash-${variant}-segv.core" 11 "$default_core_filter" \
        "$inputs" "$program" segv
    generate_core "$output_dir/crash-${variant}-abort.core" 6 "$default_core_filter" \
        "$inputs" "$program" abort
done
generate_core "$output_dir/crash-gcc-o0-headerless.core" 11 "$headerless_core_filter" \
    "$output_dir/crash-gcc-o0 $output_dir/libcrash.so" "$output_dir/crash-gcc-o0" segv
generate_core "$output_dir/crash-gcc-o0-headers-only.core" 11 "$headers_only_core_filter" \
    "$output_dir/crash-gcc-o0 $output_dir/libcrash.so" "$output_dir/crash-gcc-o0" segv
generate_core "$output_dir/crash-gcc-o0-memoryless.core" 11 "$memoryless_core_filter" \
    "$output_dir/crash-gcc-o0 $output_dir/libcrash.so" "$output_dir/crash-gcc-o0" segv
for language in rust go zig; do
    generate_core "$output_dir/crash-${language}-o0.core" 11 "$default_core_filter" \
        "$output_dir/crash-${language}-o0" "$output_dir/crash-${language}-o0"
done
# Cores whose executable or shared library was deleted after the crash. The
# copies are refreshed whenever a core itself must be regenerated.
generate_core_without() {
    local name="$1"
    local deleted="$2"
    local directory="$output_dir/core-missing-${name}"
    local core="$directory/crash.core"
    local inputs="$output_dir/crash-gcc-o0 $output_dir/libcrash.so"
    mkdir -p "$directory"
    if ! core_is_current "$core" "$(core_signature 11 "$default_core_filter" \
        "$inputs" "$directory/crash-gcc-o0" segv)"; then
        cp "$output_dir/crash-gcc-o0" "$output_dir/libcrash.so" "$directory/"
    fi
    generate_core "$core" 11 "$default_core_filter" "$inputs" "$directory/crash-gcc-o0" segv
    rm -f "$directory/$deleted"
}
generate_core_without library libcrash.so
generate_core_without executable crash-gcc-o0

printf '%s\n' "${!rebuilt_outputs[@]}" >"${suite_outputs}.tmp"
mv "${suite_outputs}.tmp" "$suite_outputs"
mv "${suite_stamp}.tmp" "$suite_stamp"
