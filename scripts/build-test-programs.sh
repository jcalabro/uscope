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
readonly frame_oracle_script=scripts/frame-variables-oracle.py

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

build_cpp_fixture_directory() {
    local compiler="$1"
    local source_dir="$2"
    local output="$3"
    shift 3
    local -a sources=()
    mapfile -d '' sources < <(
        find "$source_dir" -maxdepth 1 -type f -name '*.cpp' -print0 | sort -z
    )
    local -a command=(
        "$compiler" -std=c++20 -Wall -Wextra -Werror "$@" "${sources[@]}" -o "$output"
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

# Builds the library of the ELF symbol fixture without debug information, so
# that only its symbol tables and call-frame information describe it.
build_symbols_library() {
    local compiler="$1"
    local output="$2"
    local soname="${3:-${output##*/}}"
    local source_dir="$c_fixtures_dir/elf-symbols"
    local -a command=(
        "$compiler" -std=c17 -Wall -Wextra -Werror -shared -fPIC -O2
        "-Wl,-soname,${soname}" "$source_dir/library.c" "$source_dir/layout.S" -o "$output"
    )
    read_dash_version "$compiler"
    run_cached_build "$source_dir" "$output" \
        "compiler=${dash_version}"$'\n'"target=x86_64-linux"$'\n'"backend=${compiler}" \
        "${command[@]}"
}

# Builds the disassembly fixture from its C program and hand-written code.
# Lazy binding keeps procedure linkage table slots unresolved until first use.
build_disassembly_fixture() {
    local compiler="$1"
    local output="$2"
    shift 2
    local source_dir="$c_fixtures_dir/disassembly"
    local -a command=(
        "$compiler" -std=c17 -Wall -Wextra -Werror -g3 -gdwarf-5 -Wl,-z,lazy "$@"
        "$source_dir/main.c" "$source_dir/layout.S" "$source_dir/indirect.S" -o "$output"
    )
    read_dash_version "$compiler"
    run_cached_build "$source_dir" "$output" \
        "compiler=${dash_version}"$'\n'"target=x86_64-linux"$'\n'"backend=${compiler}" \
        "${command[@]}"
}

# Builds a program of the TLS modules fixture from the sources and flags
# given, so a static build can link the library's source in.
build_tls_modules_fixture() {
    local compiler="$1"
    local output="$2"
    shift 2
    local -a command=(
        "$compiler" -std=c17 -Wall -Wextra -Werror -g3 -gdwarf-5 -pthread "$@" -o "$output"
    )
    read_dash_version "$compiler"
    run_cached_build "$c_fixtures_dir/tls-modules" "$output" \
        "compiler=${dash_version}"$'\n'"target=x86_64-linux"$'\n'"backend=${compiler}" \
        "${command[@]}"
}

# Derives a C library standing in for another machine's build: the version
# that libthread_db checks and the build-id each differ in one byte, while the
# code stays the toolchain's own so the fixture runs against its loader.
derive_foreign_libc() {
    local input="$1"
    local output="$2"
    local script
    # shellcheck disable=SC2016
    script='
        set -euo pipefail
        input="$1"; output="$2"
        # Converts a virtual address to its file offset through the load
        # segment containing it.
        file_offset() {
            readelf -lW "$input" | awk -v address=$(( $1 )) "
                \$1 == \"LOAD\" && address >= strtonum(\$3) && address < strtonum(\$3) + strtonum(\$5) {
                    print address - strtonum(\$3) + strtonum(\$2); exit
                }"
        }
        byte_at() { od -An -tu1 -j "$1" -N1 "$output" | tr -d " "; }
        put_byte() { printf "\\$(printf %03o "$2")" | dd of="$output" bs=1 seek="$1" conv=notrunc status=none; }
        read -r version_address version_size < <(readelf -W --dyn-syms "$input" \
            | awk "\$8 ~ /^__nptl_version@/ { print \"0x\" \$2, \$3; exit }")
        version=$(file_offset "$version_address")
        # Section numbers are bracketed and padded, so they are removed first.
        note=$(readelf -SW "$input" | sed "s/^ *\\[ *[0-9]*\\]//" \
            | awk "\$1 == \".note.gnu.build-id\" { print \"0x\" \$4; exit }")
        if [[ -z "$version" || -z "$note" ]]; then
            printf "error: %s has no __nptl_version or build-id note\n" "$input" >&2
            exit 1
        fi
        cp "$input" "$output.tmp"
        chmod u+w "$output.tmp"
        output="$output.tmp"
        if [[ "$(dd if="$output" bs=1 skip="$version" count="$version_size" status=none | tr -d "\0")" != [0-9]*.* ]]; then
            printf "error: %s does not hold a version at __nptl_version\n" "$input" >&2
            exit 1
        fi
        # The major version becomes 9, or 8 when it already is 9.
        major=$(byte_at "$version")
        put_byte "$version" $(( major == 57 ? 56 : 57 ))
        # The build-id descriptor follows the 12-byte note header and "GNU".
        identifier=$(( note + 16 ))
        put_byte "$identifier" $(( $(byte_at "$identifier") ^ 255 ))
        mv "$output" "${output%.tmp}"
    '
    run_cached_build "$input" "$output" "derivation=foreign-libc-v1" \
        bash -c "$script" _ "$input" "$output"
}

# Derives a library with only a dynamic symbol table from one with full
# symbol tables. With an embedded table, the result also carries a
# MiniDebugInfo section built the way Fedora's find-debuginfo does: the
# compressed object keeps exactly the function symbols the dynamic table
# omits. The uncompressed object is kept beside the library for oracles.
derive_stripped_library() {
    local input="$1"
    local output="$2"
    local embedded="$3"
    local script
    # shellcheck disable=SC2016
    script='
        set -euo pipefail
        input="$1"; output="$2"; embedded="$3"
        rm -f "$output" "$output.embedded" "$output.embedded.xz"
        strip --strip-all -o "$output" "$input"
        if [[ "$embedded" == yes ]]; then
            nm -D "$input" --format=posix --defined-only | awk "{ print \$1 }" | sort >"$output.dynsyms"
            nm "$input" --format=posix --defined-only \
                | awk "{ if (\$2 == \"T\" || \$2 == \"t\" || \$2 == \"D\") print \$1 }" \
                | sort >"$output.funcsyms"
            comm -13 "$output.dynsyms" "$output.funcsyms" >"$output.keep"
            objcopy --only-keep-debug "$input" "$output.embedded"
            objcopy -S --remove-section .gdb_index --remove-section .comment \
                "--keep-symbols=$output.keep" "$output.embedded"
            xz --keep "$output.embedded"
            objcopy --add-section ".gnu_debugdata=$output.embedded.xz" "$output"
            rm -f "$output.dynsyms" "$output.funcsyms" "$output.keep" "$output.embedded.xz"
        fi
    '
    run_cached_build "$input" "$output" \
        "derivation=stripped-v1"$'\n'"embedded=${embedded}" \
        bash -c "$script" _ "$input" "$output" "$embedded"
}

# Splits the debug information off a program or library as distributions
# ship it, stripping every symbol table but the dynamic one from OUTPUT. With
# LAYOUT `debuglink`, OUTPUT names its debug file by `.gnu_debuglink`, which
# sits beside it under `.debug`; with `build-id`, the debug file is filed
# under ROOT/.build-id by the build-id OUTPUT keeps.
derive_split_debug() {
    local input="$1"
    local output="$2"
    local layout="$3"
    local root="${4:-}"
    local script
    # shellcheck disable=SC2016
    script='
        set -euo pipefail
        input="$1"; output="$2"; layout="$3"; root="$4"
        objcopy --only-keep-debug "$input" "$output.debug.tmp"
        strip --strip-all -o "$output.tmp" "$input"
        case "$layout" in
            debuglink)
                directory="$(dirname "$output")/.debug"
                mkdir -p "$directory"
                debug="$directory/$(basename "$output").debug"
                mv "$output.debug.tmp" "$debug"
                objcopy "--add-gnu-debuglink=$debug" "$output.tmp"
                ;;
            build-id)
                id=$(readelf -n "$input" | awk "/Build ID:/ { print \$3 }")
                [[ -n "$id" ]] || { echo "$input has no build-id" >&2; exit 1; }
                mkdir -p "$root/.build-id/${id:0:2}"
                mv "$output.debug.tmp" "$root/.build-id/${id:0:2}/${id:2}.debug"
                ;;
        esac
        mv "$output.tmp" "$output"
    '
    run_cached_build "$input" "$output" \
        "derivation=split-debug-v1"$'\n'"layout=${layout}"$'\n'"root=${root}" \
        bash -c "$script" _ "$input" "$output" "$layout" "$root"
}

# Fails the build when the symbol fixture library no longer has the symbol
# tables and layout the symbolization tests depend on. TABLES names which
# tables must exist: full, dynamic, or embedded.
require_symbols_layout() {
    local library="$1"
    local tables="$2"
    local sections symbols
    sections=$(readelf -SW "$library")
    symbols=$(readelf -sW "$library")
    fail() {
        printf 'error: %s: %s\n' "$library" "$1" >&2
        exit 1
    }
    if grep -F .debug_info <<<"$sections" >/dev/null; then
        fail "has DWARF debug information"
    fi
    local has_symtab=no has_embedded=no
    grep -F ' .symtab ' <<<"$sections" >/dev/null && has_symtab=yes
    grep -F ' .gnu_debugdata ' <<<"$sections" >/dev/null && has_embedded=yes
    case "$tables" in
        full) [[ $has_symtab == yes && $has_embedded == no ]] || fail "lacks a static symbol table" ;;
        dynamic) [[ $has_symtab == no && $has_embedded == no ]] || fail "is not stripped" ;;
        embedded) [[ $has_symtab == no && $has_embedded == yes ]] || fail "lacks MiniDebugInfo" ;;
    esac
    local layout_symbols="$symbols"
    if [[ "$tables" == embedded ]]; then
        layout_symbols=$(readelf -sW "$library.embedded")
        if ! grep -E ' FUNC +LOCAL .* lib_static_helper$' <<<"$layout_symbols" >/dev/null; then
            fail "MiniDebugInfo lacks lib_static_helper"
        fi
        if grep -E ' asm_sized$' <<<"$layout_symbols" >/dev/null; then
            fail "MiniDebugInfo repeats exported symbols"
        fi
    fi
    if [[ "$tables" == dynamic || "$tables" == embedded ]] \
        && grep -F lib_static_helper <<<"$symbols" >/dev/null; then
        fail "still names lib_static_helper"
    fi
    # The return address of the call ending asm_noreturn_caller must be the
    # first byte of asm_after_noreturn, and asm_nested_inner must lie strictly
    # inside asm_nested_outer.
    awk '
        $8 == "asm_noreturn_caller" { caller_end = strtonum("0x" $2) + $3 }
        $8 == "asm_after_noreturn" { after = strtonum("0x" $2) }
        $8 == "asm_nested_outer" { outer = strtonum("0x" $2); outer_end = outer + $3 }
        $8 == "asm_nested_inner" { inner = strtonum("0x" $2); inner_end = inner + $3 }
        END {
            exit !(caller_end == after && outer < inner && inner_end < outer_end)
        }' <<<"$symbols" || fail "asm layout changed"
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

# Reads the Go toolchain's version and target once.
read_go_version() {
    if [[ -z "$go_version" ]]; then
        go_version=$(go version)
        go_target=$(go env GOOS GOARCH)
        go_target=${go_target//$'\n'//}
    fi
}

# Builds a Go package without cgo. GO_CGO=1 enables cgo, which an external
# link needs; GO_CC and GO_CFLAGS then choose its C compiler and flags.
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
        env "CGO_ENABLED=${GO_CGO:-0}" ${GO_CC:+"CC=$GO_CC"} ${GO_CFLAGS:+"CGO_CFLAGS=$GO_CFLAGS"}
        go build -buildvcs=false "$@" -o "$output" "${sources[@]}"
    )
    read_go_version
    run_cached_build "$package_dir" "$output" \
        "compiler=${go_version}"$'\n'"target=${go_target}"$'\n'"backend=gc" \
        "${command[@]}"
}

# Builds a command from the pinned Go toolchain's own sources.
build_go_command() {
    local package="$1"
    local output="$2"
    shift 2
    local -a command=(
        env CGO_ENABLED=0 go build -buildvcs=false "$@" -o "$output" "$package"
    )
    read_go_version
    run_cached_build "$(go env GOROOT)/src/$package" "$output" \
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

# Zig's own backend, which Debug builds use by default, describes optionals,
# error unions, and tagged unions as variant parts, and a type declared in
# another by its parent.
build_zig_self_hosted_fixture() {
    local source="$1"
    local output="$2"
    shift 2
    local -a command=(
        zig build-exe "$source" -target x86_64-linux-gnu -fno-llvm -fno-strip "$@"
        "-femit-bin=${output}"
    )
    if [[ -z "$zig_version" ]]; then
        zig_version=$(zig version)
    fi
    run_cached_build "$source" "$output" \
        "compiler=zig ${zig_version}"$'\n'"target=x86_64-linux-gnu"$'\n'"backend=self-hosted" \
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

# Fails the build when a function no longer contains an instruction a test
# depends on, such as a repeated string store or a 16-byte vector store.
require_instruction() {
    local output="$1"
    local function="$2"
    local pattern="$3"
    local key=${pattern//[^a-zA-Z0-9]/_}
    local stamp="${output}.validation-instruction-${function}-${key}"
    local signature="validator=instruction-v1"$'\n'"function=${function}"$'\n'"pattern=${pattern}"
    if validation_is_cached "$output" "$stamp" "$signature"; then
        return
    fi
    # grep reads all input; see require_tail_jump.
    if ! objdump -d --no-show-raw-insn "$output" \
        | sed -n "/<${function}>:/,/^\$/p" \
        | grep -E "$pattern" >/dev/null; then
        printf 'error: %s function %s does not contain %s\n' \
            "$output" "$function" "$pattern" >&2
        exit 1
    fi
    record_validation "$stamp" "$signature"
}

# Fails the build unless a fixture runs on musl: it names musl's loader as
# its interpreter or, linked statically, contains musl's TLS layout code.
# Otherwise a toolchain that quietly targeted glibc would pass musl's tests.
require_musl() {
    local output="$1"
    local stamp="${output}.validation-musl"
    local signature="validator=musl-v1"
    if validation_is_cached "$output" "$stamp" "$signature"; then
        return
    fi
    local interpreter
    interpreter=$(readelf -lW "$output" \
        | sed -n 's/.*Requesting program interpreter: \(.*\)]$/\1/p')
    if [[ -n "$interpreter" ]]; then
        if [[ "${interpreter##*/}" != ld-musl-* ]]; then
            printf 'error: %s runs on %s, not musl\n' "$output" "$interpreter" >&2
            exit 1
        fi
    elif ! nm "$output" | grep -E ' __copy_tls$' >/dev/null; then
        printf 'error: %s is not statically linked with musl\n' "$output" >&2
        exit 1
    fi
    record_validation "$stamp" "$signature"
}

# Fails the build unless a fixture is statically linked with glibc: it has no
# interpreter and contains glibc's static TLS setup. `threads` says whether it
# must contain glibc's thread library, whose absence leaves a program without
# the descriptors libthread_db reads.
require_static_glibc() {
    local output="$1"
    local threads="$2"
    local stamp="${output}.validation-static-glibc"
    local signature="validator=static-glibc-v1"$'\n'"threads=${threads}"
    if validation_is_cached "$output" "$stamp" "$signature"; then
        return
    fi
    local symbols
    symbols=$(nm "$output")
    if readelf -lW "$output" | grep -F 'Requesting program interpreter' >/dev/null ||
        ! grep -E ' __libc_setup_tls$' <<<"$symbols" >/dev/null; then
        printf 'error: %s is not statically linked with glibc\n' "$output" >&2
        exit 1
    fi
    if grep -E ' __nptl_version$' <<<"$symbols" >/dev/null; then
        local linked=yes
    else
        local linked=no
    fi
    if [[ $linked != "$threads" ]]; then
        printf 'error: %s contains the thread library: %s, expected %s\n' \
            "$output" "$linked" "$threads" >&2
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
# inferior's coredump_filter, which gcore honors like the kernel. gdb's log,
# with everything the fixture printed, is kept as CORE.log.
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
    # gcore saves zeros for the whole of a mapping it fails to read. No
    # process can read the vsyscall page, which no test reads either.
    if grep -F 'Memory read failed for corefile section' <<<"$log" |
        grep -Fv ' at 0xffffffffff600000.' >/dev/null; then
        printf 'error: gcore could not read all of %s:\n%s\n' "$temporary" "$log" >&2
        rm -f "$temporary"
        exit 1
    fi
    mv "$temporary" "$core"
    printf '%s\n' "$log" >"${core}.log"
    rebuilt_outputs["$core"]=true
    printf '%s\n' "$signature" >"${stamp}.tmp"
    mv "${stamp}.tmp" "$stamp"
}

# Nix store paths identify each toolchain, so their resolved paths and mtimes
# change with compiler versions without spawning version probes.
suite_signature() {
    local -a paths=()
    local tool path
    for tool in gcc g++ clang clang++ clang++-libc++ musl-gcc musl-clang rustc cargo go zig objdump \
        gdb setarch; do
        if path=$(type -P "$tool"); then
            paths+=("$path")
        fi
    done
    # Statically linked glibc fixtures link from a store path of their own.
    printf 'suite-v1\nGOOS=%s GOARCH=%s\nGLIBC_STATIC_LIBRARIES=%s\nUSCOPE_FIXTURE_CRATES=%s\n' \
        "${GOOS-}" "${GOARCH-}" "$GLIBC_STATIC_LIBRARIES" "${USCOPE_FIXTURE_CRATES-}"
    stat -L --format='%n %Y' "${paths[@]}"
}

# Skips every per-fixture probe when no fixture source, SDK, kernel, this
# script, or tool changed and every previously built output still exists.
suite_is_current() {
    local signature="$1"
    [[ -f "$suite_stamp" && -f "$suite_outputs" ]] || return 1
    [[ "$(<"$suite_stamp")" == "$signature" ]] || return 1
    [[ -z "$(find "$fixtures_dir" sdk views/kernels scripts/gosym-oracle \
        scripts/coroutine-oracle.awk "${BASH_SOURCE[0]}" \
        "$frame_oracle_script" -newer "$suite_stamp" -print -quit)" ]] \
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
# Built as if elsewhere: its debug information names sources under a
# directory that does not exist here.
build_fixture gcc "$c_fixtures_dir/basic.c" "$output_dir/basic-relocated" \
    -O0 -g3 -fPIE -pie "-ffile-prefix-map=${PWD}=/nonexistent/uscope"
build_fixture gcc "$c_fixtures_dir/attach.c" "$output_dir/attach" \
    -O0 -g3 -fPIE -pie
build_fixture gcc "$c_fixtures_dir/attach-threads.c" "$output_dir/attach-threads" \
    -O0 -g3 -fPIE -pie -pthread
build_fixture gcc "$c_fixtures_dir/attach-clones.c" "$output_dir/attach-clones" \
    -O0 -g3 -fPIE -pie -pthread
build_fixture gcc "$c_fixtures_dir/attach-exited-leader.c" "$output_dir/attach-exited-leader" \
    -O0 -g3 -fPIE -pie -pthread
build_fixture gcc "$c_fixtures_dir/attach-restart.c" "$output_dir/attach-restart" \
    -O0 -g3 -fPIE -pie
build_fixture gcc "$c_fixtures_dir/attach-leader-exits.c" "$output_dir/attach-leader-exits" \
    -O0 -g3 -fPIE -pie -pthread
build_fixture gcc "$c_fixtures_dir/attach-fork.c" "$output_dir/attach-fork" \
    -O0 -g3 -fPIE -pie
build_fixture gcc "$c_fixtures_dir/exited-leader.c" "$output_dir/exited-leader" \
    -O0 -g3 -fPIE -pie -pthread
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
build_fixture gcc "$c_fixtures_dir/command-names.c" "$output_dir/command-names" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie
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
for variant in "gcc -O0" "gcc -O2" "clang -O2"; do
    read -r compiler level <<<"$variant"
    suffix="${level#-}"
    build_fixture "$compiler" "$c_fixtures_dir/realigned.c" \
        "$output_dir/realigned-${compiler}-${suffix,,}" "$level" -g3 -gdwarf-5 -fPIE -pie
done
for compiler in gcc clang; do
    build_fixture "$compiler" "$c_fixtures_dir/function-types.c" \
        "$output_dir/function-types-${compiler}-o0" -O0 -g3 -gdwarf-5 -fPIE -pie
done
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
for variant in "gcc -O0" "gcc -O2" "clang -O0" "clang -O2"; do
    read -r compiler level <<<"$variant"
    suffix="${level#-}"
    build_fixture "$compiler" "$c_fixtures_dir/returns.c" \
        "$output_dir/returns-c-${compiler}-${suffix,,}" "$level" -g3 -gdwarf-5 -fPIE -pie
done
build_fixture gcc "$c_fixtures_dir/pieces.c" "$output_dir/pieces-gcc-o0" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie
build_fixture gcc "$c_fixtures_dir/pieces.c" "$output_dir/pieces-gcc-o2" \
    -O2 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie
require_dwarf_operation "$output_dir/pieces-gcc-o2" 'DW_OP_implicit_value.*DW_OP_piece'
require_dwarf_operation "$output_dir/pieces-gcc-o2" 'DW_OP_piece: 8; DW_OP_piece: 8'
build_fixture clang "$c_fixtures_dir/pieces.c" "$output_dir/pieces-clang-o2" \
    -O2 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie
require_dwarf_operation "$output_dir/pieces-clang-o2" 'DW_OP_reg14 (r14); DW_OP_piece'
build_fixture gcc "$c_fixtures_dir/variables-static.c" "$output_dir/variables-static-gcc-o2" \
    -O2 -g3 -gdwarf-5 -fomit-frame-pointer -fPIE -pie
build_fixture clang "$c_fixtures_dir/variables-static.c" "$output_dir/variables-static-clang-o2" \
    -O2 -g3 -gdwarf-5 -fomit-frame-pointer -fPIE -pie
require_dwarf_operation "$output_dir/variables-static-clang-o2" DW_OP_addrx
build_fixture gcc "$c_fixtures_dir/variables-static.c" "$output_dir/variables-static-gcc-nopie" \
    -O2 -g3 -gdwarf-5 -fomit-frame-pointer -no-pie
require_dwarf_operation "$output_dir/variables-static-gcc-nopie" 'DW_OP_addr:'
for compiler in gcc clang; do
    for optimization in o0 o2; do
        build_c_fixture_directory "$compiler" "$c_fixtures_dir/expressions" \
            "$output_dir/expressions-c-$compiler-$optimization-pie" \
            "-${optimization^^}" -g3 -gdwarf-5 -fPIE -pie
        build_c_fixture_directory "$compiler" "$c_fixtures_dir/expressions" \
            "$output_dir/expressions-c-$compiler-$optimization-nopie" \
            "-${optimization^^}" -g3 -gdwarf-5 -fno-pie -no-pie
    done
done
build_c_fixture_directory gcc "$c_fixtures_dir/same-names" "$output_dir/same-names" \
    -O0 -g3 -gdwarf-5 -fPIE -pie
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
# A program and a library that carry views in .debug_uscope_views, which
# the assembler reads from the fixture's directory, so the cache watches the
# directory whole.
embedded_views_dir="$c_fixtures_dir/embedded-views"
read_dash_version gcc
embedded_views_metadata="compiler=${dash_version}"$'\n'"target=x86_64-linux"$'\n'"backend=gcc"
run_cached_build "$embedded_views_dir" "$output_dir/libembedded-views.so" \
    "$embedded_views_metadata" \
    gcc -std=c17 -Wall -Wextra -Werror -shared -fPIC -O0 -g3 -gdwarf-5 -Isdk/c \
    "$embedded_views_dir/library.c" -o "$output_dir/libembedded-views.so"
run_cached_build "$embedded_views_dir" "$output_dir/embedded-views" \
    "$embedded_views_metadata" \
    gcc -std=c17 -Wall -Wextra -Werror -O0 -g3 -gdwarf-5 -fPIE -pie -Isdk/c \
    "$embedded_views_dir/main.c" -o "$output_dir/embedded-views" \
    "-L$output_dir" -lembedded-views '-Wl,-rpath,$ORIGIN'
# The kernels uscope carries must be what their sources build, so that each
# is reviewed as its source; zig caches the build.
zig build-exe -target wasm32-freestanding -O ReleaseSmall -fno-entry -rdynamic \
    --stack 16384 --dep uscope_kernel -Mroot=views/kernels/rust-btree.zig \
    -Muscope_kernel=sdk/zig/uscope_kernel.zig -femit-bin="$output_dir/rust-btree.wasm"
if ! cmp -s "$output_dir/rust-btree.wasm" views/kernels/rust-btree.wasm; then
    printf 'views/kernels/rust-btree.wasm is not what its source builds: copy %s there\n' \
        "$output_dir/rust-btree.wasm" >&2
    exit 1
fi
rebuilt_outputs["$output_dir/rust-btree.wasm"]=true
# The program docs/writing-views.md writes views for, which carries them, and
# the kernel one of them calls, written with the C SDK.
zig cc --target=wasm32-freestanding -Os -nostdlib -Wl,--no-entry -Wl,-z,stack-size=16384 \
    -Isdk/c "$c_fixtures_dir/tutorial/tree.c" -o "$output_dir/tutorial-tree.wasm"
rebuilt_outputs["$output_dir/tutorial-tree.wasm"]=true
run_cached_build "$c_fixtures_dir/tutorial" "$output_dir/tutorial" \
    "$embedded_views_metadata" \
    gcc -std=c17 -Wall -Wextra -Werror -O0 -g3 -gdwarf-5 -fPIE -pie -Isdk/c "-Wa,-I$output_dir" \
    "$c_fixtures_dir/tutorial/tutorial.c" -o "$output_dir/tutorial"
build_shared_fixture gcc "$c_fixtures_dir/moved-code/library.c" "$output_dir/libmoved-code.so" \
    -O0 -g3 -gdwarf-5
build_fixture gcc "$c_fixtures_dir/moved-code/main.c" "$output_dir/moved-code" \
    -O0 -g3 -gdwarf-5 -fPIE -pie "-L$output_dir" -lmoved-code '-Wl,-rpath,$ORIGIN'
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
# Programs and a library whose debug information is a separate file, found
# by `.gnu_debuglink` beside them, by build-id under a debug directory, or
# from a debuginfod server.
mkdir -p "$output_dir/split"
build_fixture gcc "$c_fixtures_dir/basic.c" "$output_dir/split/basic-debuglink.full" \
    -O0 -g3 -gdwarf-5 -fPIE -pie
derive_split_debug "$output_dir/split/basic-debuglink.full" "$output_dir/split/basic-debuglink" \
    debuglink
build_fixture gcc "$c_fixtures_dir/basic.c" "$output_dir/split/basic-build-id.full" \
    -O0 -g3 -gdwarf-5 -fPIE -pie -Wl,--build-id
derive_split_debug "$output_dir/split/basic-build-id.full" "$output_dir/split/basic-build-id" \
    build-id "$output_dir/split/debug-root"
# The same debug file under another debug directory, naming a dwz
# supplementary file as distributions' debug files do, which uscope refuses
# with its reason rather than misread.
altlinked_script='
    set -euo pipefail
    root="$1"; output="$2"
    rm -rf "$output"
    cp -r "$root" "$output"
    debug=$(find "$output/.build-id" -name "*.debug")
    printf "/usr/lib/debug/.dwz/uscope-fixture\0\x01\x02\x03\x04" >"$output/altlink"
    objcopy --add-section ".gnu_debugaltlink=$output/altlink" "$debug"
    rm "$output/altlink"
    # The build cache counts only an executable output as built.
    touch "$output/ready"
    chmod +x "$output/ready"
'
run_cached_build "$output_dir/split/basic-build-id" "$output_dir/split/altlink-root/ready" \
    "derivation=altlinked-v1" \
    bash -c "$altlinked_script" _ "$output_dir/split/debug-root" "$output_dir/split/altlink-root"
build_shared_fixture gcc "$c_fixtures_dir/module-frames/library.c" \
    "$output_dir/split/libmodule-frames.so.full" -O0 -g3 -gdwarf-5 -Wl,--build-id \
    -Wl,-soname,libmodule-frames.so
derive_split_debug "$output_dir/split/libmodule-frames.so.full" \
    "$output_dir/split/libmodule-frames.so" debuglink
build_fixture gcc "$c_fixtures_dir/module-frames/main.c" "$output_dir/split/module-frames" \
    -O0 -g3 -gdwarf-5 -fPIE -pie "-L$output_dir/split" -lmodule-frames '-Wl,-rpath,$ORIGIN'
build_symbols_library gcc "$output_dir/libelf-symbols-gcc.so"
build_symbols_library clang "$output_dir/libelf-symbols-clang.so"
# Stripped libraries keep the soname they were linked with, so each derived
# library starts from a full build that already carries its final soname.
for library in stripped minidebug; do
    build_symbols_library gcc "$output_dir/libelf-symbols-${library}.so.full" \
        "libelf-symbols-${library}.so"
done
derive_stripped_library "$output_dir/libelf-symbols-stripped.so.full" \
    "$output_dir/libelf-symbols-stripped.so" no
derive_stripped_library "$output_dir/libelf-symbols-minidebug.so.full" \
    "$output_dir/libelf-symbols-minidebug.so" yes
require_symbols_layout "$output_dir/libelf-symbols-gcc.so" full
require_symbols_layout "$output_dir/libelf-symbols-clang.so" full
require_symbols_layout "$output_dir/libelf-symbols-stripped.so" dynamic
require_symbols_layout "$output_dir/libelf-symbols-minidebug.so" embedded
readonly symbols_variants=(
    "gcc-o0 gcc gcc -O0 -fno-omit-frame-pointer -fPIE -pie"
    "clang-o2 clang clang -O2 -fomit-frame-pointer -fPIE -pie"
    "gcc-nopie gcc gcc -O2 -fomit-frame-pointer -no-pie"
    "stripped stripped gcc -O0 -fno-omit-frame-pointer -fPIE -pie"
    "minidebug minidebug gcc -O0 -fno-omit-frame-pointer -fPIE -pie"
)
for variant in "${symbols_variants[@]}"; do
    read -r name library compiler flags <<<"$variant"
    # shellcheck disable=SC2086
    build_fixture "$compiler" "$c_fixtures_dir/elf-symbols/main.c" \
        "$output_dir/elf-symbols-${name}" -g3 -gdwarf-5 $flags \
        "-L$output_dir" "-lelf-symbols-${library}" '-Wl,-rpath,$ORIGIN'
    if ! readelf -dW "$output_dir/elf-symbols-${name}" \
        | grep -F "Shared library: [libelf-symbols-${library}.so]" >/dev/null; then
        printf 'error: elf-symbols-%s does not load libelf-symbols-%s.so\n' \
            "$name" "$library" >&2
        exit 1
    fi
done
build_disassembly_fixture gcc "$output_dir/disassembly-gcc-o0" \
    -O0 -fno-omit-frame-pointer -fPIE -pie
build_disassembly_fixture clang "$output_dir/disassembly-clang-o2-nopie" \
    -O2 -fomit-frame-pointer -no-pie
build_fixture gcc "$c_fixtures_dir/null-call.c" "$output_dir/null-call" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie
build_fixture gcc "$c_fixtures_dir/tls.c" "$output_dir/globals-tls-gcc" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie -pthread
build_fixture clang "$c_fixtures_dir/tls.c" "$output_dir/globals-tls-clang" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie -pthread
# TLS in an executable, a linked library, and a dlopen'd plugin, on glibc and
# on musl. Statically linked builds have one module and no plugin.
readonly tls_modules_dir="$c_fixtures_dir/tls-modules"
for libc in glibc musl; do
    compiler=gcc suffix=""
    if [[ $libc == musl ]]; then
        compiler=musl-gcc suffix=-musl
    fi
    build_shared_fixture "$compiler" "$tls_modules_dir/library.c" \
        "$output_dir/libtls-modules${suffix}.so" -O0 -g3 -gdwarf-5
    build_shared_fixture "$compiler" "$tls_modules_dir/plugin.c" \
        "$output_dir/libtls-plugin${suffix}.so" -O0 -g3 -gdwarf-5
done
tls_modules_linked=("-L$output_dir" '-Wl,-rpath,$ORIGIN')
build_tls_modules_fixture gcc "$output_dir/tls-modules-gcc" -O0 -fPIE -pie \
    '-DPLUGIN="libtls-plugin.so"' "$tls_modules_dir/main.c" "${tls_modules_linked[@]}" \
    -ltls-modules
build_tls_modules_fixture musl-gcc "$output_dir/tls-modules-musl-gcc-o0" -O0 -fPIE -pie \
    '-DPLUGIN="libtls-plugin-musl.so"' "$tls_modules_dir/main.c" "${tls_modules_linked[@]}" \
    -ltls-modules-musl
build_tls_modules_fixture musl-clang "$output_dir/tls-modules-musl-clang-o2-nopie" -O2 -no-pie \
    '-DPLUGIN="libtls-plugin-musl.so"' "$tls_modules_dir/main.c" "${tls_modules_linked[@]}" \
    -ltls-modules-musl
build_tls_modules_fixture musl-gcc "$output_dir/tls-modules-musl-gcc-static" -O0 -static \
    -DSTATIC_BUILD "$tls_modules_dir/main.c" "$tls_modules_dir/library.c"
build_tls_modules_fixture musl-clang "$output_dir/tls-modules-musl-clang-static-pie" -O2 \
    -static-pie -fPIE -DSTATIC_BUILD "$tls_modules_dir/main.c" "$tls_modules_dir/library.c"
for variant in gcc-o0 clang-o2-nopie gcc-static clang-static-pie; do
    require_musl "$output_dir/tls-modules-musl-${variant}"
done
# Statically linked glibc, with and without its thread library.
glibc_static=(-L"$GLIBC_STATIC_LIBRARIES" -DSTATIC_BUILD "$tls_modules_dir/library.c")
build_tls_modules_fixture gcc "$output_dir/tls-modules-gcc-static" -O0 -static \
    "$tls_modules_dir/main.c" "${glibc_static[@]}"
build_tls_modules_fixture clang "$output_dir/tls-modules-clang-static-pie" -O2 \
    -static-pie -fPIE "$tls_modules_dir/main.c" "${glibc_static[@]}"
build_tls_modules_fixture gcc "$output_dir/tls-modules-single-thread-gcc-static-pie" -O0 \
    -static-pie -fPIE "$tls_modules_dir/single-thread.c" "${glibc_static[@]}"
build_tls_modules_fixture clang "$output_dir/tls-modules-single-thread-clang-static" -O2 \
    -static "$tls_modules_dir/single-thread.c" "${glibc_static[@]}"
require_static_glibc "$output_dir/tls-modules-gcc-static" yes
require_static_glibc "$output_dir/tls-modules-clang-static-pie" yes
require_static_glibc "$output_dir/tls-modules-single-thread-gcc-static-pie" no
require_static_glibc "$output_dir/tls-modules-single-thread-clang-static" no
for variant in "g++ gcc -O0" "g++ gcc -O2" "clang++ clang -O2"; do
    read -r compiler name level <<<"$variant"
    suffix="${level#-}"
    build_cpp_fixture "$compiler" "$cpp_fixtures_dir/returns.cpp" \
        "$output_dir/returns-cpp-${name}-${suffix,,}" "$level" -g3 -gdwarf-5 -fPIE -pie
done
build_cpp_fixture g++ "$cpp_fixtures_dir/variables.cpp" "$output_dir/variables-cpp-gcc-o0" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie
build_cpp_fixture g++ "$cpp_fixtures_dir/overloads.cpp" "$output_dir/overloads-cpp-gcc-o0" \
    -O0 -g3 -gdwarf-5 -fPIE -pie
build_cpp_fixture g++ "$cpp_fixtures_dir/overloads.cpp" "$output_dir/overloads-cpp-gcc-nodebug" \
    -O0 -fPIE -pie
build_cpp_fixture clang++ "$cpp_fixtures_dir/variables.cpp" "$output_dir/variables-cpp-clang-o0" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie
build_cpp_fixture g++ "$cpp_fixtures_dir/variables.cpp" "$output_dir/variables-cpp-gcc-o2" \
    -O2 -g3 -gdwarf-5 -fomit-frame-pointer -fPIE -pie
build_cpp_fixture clang++ "$cpp_fixtures_dir/variables.cpp" "$output_dir/variables-cpp-clang-o2" \
    -O2 -g3 -gdwarf-5 -fomit-frame-pointer -fPIE -pie
build_cpp_fixture_directory g++ "$cpp_fixtures_dir/expressions" \
    "$output_dir/expressions-cpp-gcc-o0" -O0 -g3 -gdwarf-5 -fPIE -pie
build_cpp_fixture_directory clang++ "$cpp_fixtures_dir/expressions" \
    "$output_dir/expressions-cpp-clang-o2" -O2 -g3 -gdwarf-5 -fPIE -pie
build_cpp_fixture g++ "$cpp_fixtures_dir/records.cpp" "$output_dir/records-cpp-gcc-o0" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie
build_cpp_fixture clang++ "$cpp_fixtures_dir/records.cpp" "$output_dir/records-cpp-clang-o0" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie
build_cpp_fixture g++ "$cpp_fixtures_dir/records.cpp" "$output_dir/records-cpp-gcc-o2" \
    -O2 -g3 -gdwarf-5 -fomit-frame-pointer -fPIE -pie
build_cpp_fixture clang++ "$cpp_fixtures_dir/records.cpp" "$output_dir/records-cpp-clang-o2" \
    -O2 -g3 -gdwarf-5 -fomit-frame-pointer -fPIE -pie
# libstdc++ only declares std::string in programs that use it; clang's
# standalone debug information defines it, so its layout is known.
build_cpp_fixture clang++ "$cpp_fixtures_dir/strings.cpp" "$output_dir/strings-cpp-clang-o0" \
    -O0 -g3 -gdwarf-5 -fstandalone-debug -fno-omit-frame-pointer -fPIE -pie
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
build_cpp_fixture g++ "$cpp_fixtures_dir/templates.cpp" "$output_dir/templates-cpp-gcc-o0" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie
build_cpp_fixture clang++ "$cpp_fixtures_dir/templates.cpp" "$output_dir/templates-cpp-clang-o0" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie
# DWARF 4 type units, without DWARF 5's marks on inline namespaces.
build_cpp_fixture g++ "$cpp_fixtures_dir/templates.cpp" "$output_dir/templates-cpp-gcc-dwarf4" \
    -O0 -g3 -gdwarf-4 -fdebug-types-section -fno-omit-frame-pointer -fPIE -pie
require_dwarf_operation "$output_dir/templates-cpp-gcc-dwarf4" 'DW_AT_type.*signature:'
# LLVM's libc++ lays out and names the standard library differently.
build_cpp_fixture clang++-libc++ "$cpp_fixtures_dir/templates.cpp" "$output_dir/templates-cpp-libcxx-o0" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie
require_dwarf_operation "$output_dir/templates-cpp-libcxx-o0" 'DW_AT_name.*: __1$'
# The containers the built-in views present, across the libraries' matrix,
# in C++23 for its flat maps and std::expected.
for optimization in o0 o2; do
    level="-O${optimization#o}"
    build_cpp_fixture g++ "$cpp_fixtures_dir/containers.cpp" \
        "$output_dir/containers-cpp-gcc-$optimization" "$level" -std=c++23 -g3 -gdwarf-5 -fPIE -pie
    build_cpp_fixture clang++ "$cpp_fixtures_dir/containers.cpp" \
        "$output_dir/containers-cpp-clang-$optimization" "$level" -std=c++23 -g3 -gdwarf-5 -fPIE -pie
    build_cpp_fixture clang++-libc++ "$cpp_fixtures_dir/containers.cpp" \
        "$output_dir/containers-cpp-libcxx-$optimization" "$level" -std=c++23 -g3 -gdwarf-5 -fPIE -pie
done
# libstdc++'s copy-on-write string, from before the C++11 ABI.
build_cpp_fixture g++ "$cpp_fixtures_dir/containers.cpp" "$output_dir/containers-cpp-gcc-oldabi" \
    -O0 -std=c++23 -g3 -gdwarf-5 -fPIE -pie -D_GLIBCXX_USE_CXX11_ABI=0
# libstdc++'s debug mode, whose containers wrap the ordinary ones.
build_cpp_fixture g++ "$cpp_fixtures_dir/containers.cpp" "$output_dir/containers-cpp-gcc-debug" \
    -O0 -std=c++23 -g3 -gdwarf-5 -fPIE -pie -D_GLIBCXX_DEBUG
# libstdc++ linked into the program.
build_cpp_fixture g++ "$cpp_fixtures_dir/containers.cpp" "$output_dir/containers-cpp-gcc-static" \
    -O0 -std=c++23 -g3 -gdwarf-5 -fPIE -pie -static-libstdc++
# Template names without their arguments, which only the arguments'
# entries give.
build_cpp_fixture clang++ "$cpp_fixtures_dir/containers.cpp" "$output_dir/containers-cpp-clang-simple" \
    -O0 -std=c++23 -g3 -gdwarf-5 -gsimple-template-names -fPIE -pie
# Only with -fstandalone-debug does clang describe the libc++ classes the
# program never defines itself, such as a shared_ptr's control block.
build_cpp_fixture clang++-libc++ "$cpp_fixtures_dir/containers.cpp" \
    "$output_dir/containers-cpp-libcxx-standalone" -O0 -std=c++23 -g3 -gdwarf-5 -fstandalone-debug -fPIE -pie
build_rust_fixture "$rust_fixtures_dir/variables.rs" "$output_dir/variables-rust-o0" \
    -C opt-level=0 -C force-frame-pointers=yes
build_rust_fixture "$rust_fixtures_dir/variables.rs" "$output_dir/variables-rust-o2" \
    -C opt-level=2 -C force-frame-pointers=no
build_rust_fixture "$rust_fixtures_dir/records.rs" "$output_dir/records-rust-o0" \
    -C opt-level=0 -C force-frame-pointers=yes
build_rust_fixture "$rust_fixtures_dir/records.rs" "$output_dir/records-rust-o2" \
    -C opt-level=2 -C force-frame-pointers=no
build_rust_fixture "$rust_fixtures_dir/strings.rs" "$output_dir/strings-rust-o0" \
    -C opt-level=0 -C force-frame-pointers=yes
build_rust_fixture "$rust_fixtures_dir/floats.rs" "$output_dir/floats-rust" \
    -C opt-level=0 -C force-frame-pointers=yes
build_rust_fixture "$rust_fixtures_dir/enums.rs" "$output_dir/enums-rust-o0" \
    -C opt-level=0 -C force-frame-pointers=yes
build_rust_fixture "$rust_fixtures_dir/enums.rs" "$output_dir/enums-rust-o2" \
    -C opt-level=2 -C force-frame-pointers=no
build_rust_fixture "$rust_fixtures_dir/globals.rs" "$output_dir/globals-rust-o0" \
    -C opt-level=0 -C force-frame-pointers=yes
build_rust_fixture "$rust_fixtures_dir/globals.rs" "$output_dir/globals-rust-o2" \
    -C opt-level=2 -C force-frame-pointers=no
build_rust_fixture "$rust_fixtures_dir/expressions.rs" "$output_dir/expressions-rust-o0" \
    -C opt-level=0 -C force-frame-pointers=yes
build_rust_fixture "$rust_fixtures_dir/expressions.rs" "$output_dir/expressions-rust-o2" \
    -C opt-level=2 -C force-frame-pointers=no
build_rust_fixture "$rust_fixtures_dir/generics.rs" "$output_dir/generics-rust-o0" \
    -C opt-level=0 -C force-frame-pointers=yes
build_rust_fixture "$rust_fixtures_dir/generics.rs" "$output_dir/generics-rust-o2" \
    -C opt-level=2 -C force-frame-pointers=no
# The Rust SDK's macro, as a dependency of a program that carries views,
# whose directory the cache watches whole for the same reason as the C one.
read_dash_version rustc
rust_sdk_metadata="compiler=${dash_version}"$'\n'"target=x86_64-linux"$'\n'"backend=rustc"
run_cached_build sdk/rust "$output_dir/libuscope_views.rlib" "$rust_sdk_metadata" \
    rustc --edition=2024 -D warnings --crate-type rlib --crate-name uscope_views \
    sdk/rust/src/lib.rs -o "$output_dir/libuscope_views.rlib"
# A kernel written with the Rust SDK, which the next program carries; its
# target's core is built here, so it needs no other toolchain.
env RUSTFLAGS= CARGO_ENCODED_RUSTFLAGS= CARGO_TARGET_DIR=build/kernels \
    cargo build --quiet --release --target wasm32-unknown-unknown \
    -Zbuild-std=core,panic_abort \
    --manifest-path "$rust_fixtures_dir/embedded-views/kernel/Cargo.toml"
cp build/kernels/wasm32-unknown-unknown/release/tree.wasm \
    "$output_dir/embedded-views-rust-tree.wasm"
rebuilt_outputs["$output_dir/embedded-views-rust-tree.wasm"]=true
run_cached_build "$rust_fixtures_dir/embedded-views" "$output_dir/embedded-views-rust" \
    "$rust_sdk_metadata" \
    env "USCOPE_TREE_KERNEL=$output_dir/embedded-views-rust-tree.wasm" \
    rustc --edition=2024 -D warnings -C debuginfo=2 -C codegen-units=1 -C opt-level=0 \
    --crate-name embedded_views \
    --extern "uscope_views=$output_dir/libuscope_views.rlib" \
    "$rust_fixtures_dir/embedded-views/main.rs" -o "$output_dir/embedded-views-rust"
# Unlike the other Rust fixtures, the containers use std.
build_program rustc "$rust_fixtures_dir/containers.rs" "$output_dir/containers-rust-o0" \
    --edition=2024 -D warnings -C debuginfo=2 -C codegen-units=1 -C opt-level=0
build_program rustc "$rust_fixtures_dir/containers.rs" "$output_dir/containers-rust-o2" \
    --edition=2024 -D warnings -C debuginfo=2 -C codegen-units=1 -C opt-level=2
for level in 0 2; do
    build_program rustc "$rust_fixtures_dir/returns.rs" "$output_dir/returns-rust-o${level}" \
        --edition=2024 -D warnings -C debuginfo=2 -C codegen-units=1 -C opt-level="$level"
done
# Line tables only: no variables or types, so nothing to present.
build_program rustc "$rust_fixtures_dir/containers.rs" "$output_dir/containers-rust-limited" \
    --edition=2024 -D warnings -C debuginfo=limited -C codegen-units=1 -C opt-level=0
build_go_fixture "$go_fixtures_dir/expressions" "$output_dir/expressions-go-o0" \
    -buildmode=pie "-gcflags=all=-N -l"
build_go_fixture "$go_fixtures_dir/expressions" "$output_dir/expressions-go-o2" \
    -buildmode=pie
build_go_fixture "$go_fixtures_dir/variables" "$output_dir/variables-go-o0" \
    -buildmode=pie "-gcflags=all=-N -l"
build_go_fixture "$go_fixtures_dir/records" "$output_dir/records-go-o0" \
    -buildmode=pie "-gcflags=all=-N -l"
build_go_fixture "$go_fixtures_dir/records" "$output_dir/records-go-o2" \
    -buildmode=pie
build_go_fixture "$go_fixtures_dir/strings" "$output_dir/strings-go-o0" \
    -buildmode=pie "-gcflags=all=-N -l"
build_go_fixture "$go_fixtures_dir/globals" "$output_dir/globals-go-o0" \
    -buildmode=pie "-gcflags=all=-N -l"
build_go_fixture "$go_fixtures_dir/globals" "$output_dir/globals-go-o2" \
    -buildmode=pie
build_go_fixture "$go_fixtures_dir/enums" "$output_dir/enums-go-o0" \
    -buildmode=pie "-gcflags=all=-N -l"
build_go_fixture "$go_fixtures_dir/enums" "$output_dir/enums-go-o2" \
    -buildmode=pie
build_go_fixture "$go_fixtures_dir/generics" "$output_dir/generics-go-o0" \
    -buildmode=pie "-gcflags=all=-N -l"
build_go_fixture "$go_fixtures_dir/generics" "$output_dir/generics-go-o2" \
    -buildmode=pie
build_go_fixture "$go_fixtures_dir/containers" "$output_dir/containers-go-o0" \
    -buildmode=pie "-gcflags=all=-N -l"
build_go_fixture "$go_fixtures_dir/containers" "$output_dir/containers-go-o2" \
    -buildmode=pie
build_go_fixture "$go_fixtures_dir/names" "$output_dir/names-go-o0" \
    -buildmode=pie "-gcflags=all=-N -l"
build_go_fixture "$go_fixtures_dir/names" "$output_dir/names-go-o2" \
    -buildmode=pie
build_go_fixture "$go_fixtures_dir/stdlib" "$output_dir/stdlib-go-o0" \
    -buildmode=pie "-gcflags=all=-N -l"
build_go_fixture "$go_fixtures_dir/stdlib" "$output_dir/stdlib-go-o2" \
    -buildmode=pie
build_go_fixture "$go_fixtures_dir/values" "$output_dir/values-go-o0" \
    -buildmode=pie "-gcflags=all=-N -l"
build_go_fixture "$go_fixtures_dir/values" "$output_dir/values-go-o2" \
    -buildmode=pie
require_dwarf_operation "$output_dir/variables-go-o0" 'DW_AT_language.*Go'
require_dwarf_operation "$output_dir/variables-go-o0" main.inspectScalars
require_dwarf_operation "$output_dir/enums-go-o0" 'DW_TAG_constant'
build_zig_fixture "$zig_fixtures_dir/returns.zig" "$output_dir/returns-zig-o0" \
    -O Debug -fPIE -fno-omit-frame-pointer
build_zig_fixture "$zig_fixtures_dir/returns.zig" "$output_dir/returns-zig-o2" \
    -O ReleaseSafe -fPIE -fomit-frame-pointer
build_zig_self_hosted_fixture "$zig_fixtures_dir/returns.zig" "$output_dir/returns-zig-self-hosted" \
    -O Debug
build_zig_fixture "$zig_fixtures_dir/generics.zig" "$output_dir/generics-zig-o0" \
    -O Debug -fPIE -fno-omit-frame-pointer
build_zig_fixture "$zig_fixtures_dir/containers.zig" "$output_dir/containers-zig-o0" \
    -O Debug -fPIE -fno-omit-frame-pointer
build_zig_fixture "$zig_fixtures_dir/containers.zig" "$output_dir/containers-zig-o2" \
    -O ReleaseSafe -fPIE -fomit-frame-pointer
build_zig_self_hosted_fixture "$zig_fixtures_dir/containers.zig" "$output_dir/containers-zig-self-hosted" \
    -O Debug
build_zig_fixture "$zig_fixtures_dir/expressions.zig" "$output_dir/expressions-zig-o0" \
    -O Debug -fPIE -fno-omit-frame-pointer
build_zig_fixture "$zig_fixtures_dir/expressions.zig" "$output_dir/expressions-zig-o2" \
    -O ReleaseFast -fPIE -fomit-frame-pointer
build_zig_fixture "$zig_fixtures_dir/floats.zig" "$output_dir/floats-zig" -O Debug
build_zig_fixture "$zig_fixtures_dir/variables.zig" "$output_dir/variables-zig-o0" \
    -O Debug -fPIE -fno-omit-frame-pointer
build_zig_fixture "$zig_fixtures_dir/variables.zig" "$output_dir/variables-zig-o2" \
    -O ReleaseFast -fPIE -fomit-frame-pointer
build_zig_fixture "$zig_fixtures_dir/records.zig" "$output_dir/records-zig-o0" \
    -O Debug -fPIE -fno-omit-frame-pointer
build_zig_fixture "$zig_fixtures_dir/records.zig" "$output_dir/records-zig-o2" \
    -O ReleaseFast -fPIE -fomit-frame-pointer
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
build_fixture gcc "$c_fixtures_dir/process-environment.c" "$output_dir/process-environment" \
    -O0 -g3 -fPIE -pie
build_fixture gcc "$c_fixtures_dir/output-streams.c" "$output_dir/output-streams" \
    -O0 -g3 -fPIE -pie
build_fixture gcc "$c_fixtures_dir/kvstore.c" "$output_dir/kvstore" \
    -O0 -g3 -fPIE -pie -pthread
build_program rustc "$rust_fixtures_dir/kvstore.rs" "$output_dir/kvstore-rust" \
    --edition=2024 -D warnings -C debuginfo=2 -C codegen-units=1 -C opt-level=0
build_go_fixture "$go_fixtures_dir/kvstore" "$output_dir/kvstore-go" \
    -buildmode=pie "-gcflags=all=-N -l"
build_fixture gcc "$c_fixtures_dir/strings.c" "$output_dir/strings-c-gcc-o0" \
    -O0 -g3 -fPIE -pie
build_fixture gcc "$c_fixtures_dir/glibc.c" "$output_dir/glibc-c-gcc-o0" \
    -O0 -g3 -fPIE -pie
build_fixture gcc "$c_fixtures_dir/glibc.c" "$output_dir/glibc-c-gcc-o2" \
    -O2 -g3 -fPIE -pie
build_fixture clang "$c_fixtures_dir/glibc.c" "$output_dir/glibc-c-clang-o0" \
    -O0 -g3 -fPIE -pie
build_fixture gcc "$c_fixtures_dir/floats.c" "$output_dir/floats-c-gcc" \
    -O0 -g3 -fPIE -pie
build_fixture clang "$c_fixtures_dir/floats.c" "$output_dir/floats-c-clang" \
    -O0 -g3 -fPIE -pie
build_fixture gcc "$c_fixtures_dir/line-sliding.c" "$output_dir/line-sliding" \
    -O0 -g3 -fPIE -pie
build_fixture gcc "$c_fixtures_dir/terminate.c" "$output_dir/terminate" \
    -O0 -g3 -fPIE -pie
build_fixture gcc "$c_fixtures_dir/signal-policy.c" "$output_dir/signal-policy" \
    -O0 -g3 -fPIE -pie
build_fixture gcc "$c_fixtures_dir/signal-steps.c" "$output_dir/signal-steps" \
    -O0 -g3 -fno-omit-frame-pointer -fPIE -pie
build_fixture gcc "$c_fixtures_dir/signals.c" "$output_dir/signals" \
    -O0 -g3 -fPIE -pie
build_fixture gcc "$c_fixtures_dir/fatal-signal.c" "$output_dir/fatal-signal" \
    -O0 -g3 -fPIE -pie
build_fixture gcc "$c_fixtures_dir/interrupt.c" "$output_dir/interrupt" \
    -O0 -g3 -fPIE -pie
build_fixture gcc "$c_fixtures_dir/memfd-exec.c" "$output_dir/memfd-exec" \
    -O0 -g3 -fPIE -pie
build_fixture gcc "$c_fixtures_dir/fork.c" "$output_dir/fork" \
    -O0 -g3 -fPIE -pie
build_fixture gcc "$c_fixtures_dir/job-control.c" "$output_dir/job-control" \
    -O0 -g3 -fPIE -pie
build_fixture gcc "$c_fixtures_dir/thread-exec.c" "$output_dir/thread-exec" \
    -O0 -g3 -fPIE -pie -pthread
build_fixture gcc "$c_fixtures_dir/reexec.c" "$output_dir/reexec" \
    -O0 -g3 -fPIE -pie
# Without debug information: breakpoints resolve by symbol, an indirect
# function's once its resolver has chosen, whether the loader binds at
# startup, binds lazily, or a static program relocates itself.
build_fixture gcc "$c_fixtures_dir/measure.c" "$output_dir/measure-gcc-nodebug" \
    -O0 -fPIE -pie
build_fixture clang "$c_fixtures_dir/measure.c" "$output_dir/measure-clang-nopie-lazy" \
    -O1 -fno-pie -no-pie -Wl,-z,lazy
build_fixture gcc "$c_fixtures_dir/measure.c" "$output_dir/measure-gcc-static" \
    -O0 -static -L"$GLIBC_STATIC_LIBRARIES"
build_fixture gcc "$c_fixtures_dir/thread-steps.c" "$output_dir/thread-steps-gcc-o0" \
    -O0 -g3 -fno-omit-frame-pointer -fPIE -pie -pthread
build_fixture clang "$c_fixtures_dir/thread-steps.c" "$output_dir/thread-steps-clang-o2" \
    -O2 -g3 -fPIE -pie -pthread
build_fixture gcc "$c_fixtures_dir/hot-calls.c" "$output_dir/hot-calls" \
    -O0 -g3 -fPIE -pie -pthread
build_fixture gcc "$c_fixtures_dir/thread-stress.c" "$output_dir/thread-stress" \
    -O0 -g3 -fPIE -pie -pthread
build_fixture gcc "$c_fixtures_dir/step.c" "$output_dir/step" \
    -O0 -g3 -fno-omit-frame-pointer -fPIE -pie
for variant in "gcc -O0" "clang -O0" "gcc -O2"; do
    read -r compiler level <<<"$variant"
    suffix="${level#-}"
    build_fixture "$compiler" "$c_fixtures_dir/step-targets.c" \
        "$output_dir/step-targets-${compiler}-${suffix,,}" "$level" -g3 -gdwarf-5 -fPIE -pie
done
build_fixture gcc "$c_fixtures_dir/jump.c" "$output_dir/jump" \
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
for variant in gcc-o2:gcc:-gdwarf-5 gcc-o2-dwarf4:gcc:-gdwarf-4 clang-o2:clang:-gdwarf-5; do
    IFS=: read -r name compiler dwarf <<<"$variant"
    build_fixture "$compiler" "$c_fixtures_dir/tail-frames.c" "$output_dir/tail-frames-$name" \
        -O2 -g3 "$dwarf" -fomit-frame-pointer -fPIE -pie
    require_tail_jump "$output_dir/tail-frames-$name" top middle
    require_tail_jump "$output_dir/tail-frames-$name" middle leaf
    require_tail_jump "$output_dir/tail-frames-$name" either leaf
done
build_fixture gcc "$c_fixtures_dir/step-over-libc.c" "$output_dir/step-over-libc" \
    -O0 -g3 -fno-omit-frame-pointer -fPIE -pie
build_fixture gcc "$c_fixtures_dir/orphan-frames.c" "$output_dir/orphan-frames" \
    -O0 -g3 -fno-omit-frame-pointer -fPIE -pie
# Keeps the assembly after the function before it, as the source places it.
build_fixture gcc "$c_fixtures_dir/assembly-after-code.c" "$output_dir/assembly-after-code" \
    -O0 -g3 -fno-toplevel-reorder -fno-omit-frame-pointer -fPIE -pie
build_fixture gcc "$c_fixtures_dir/undescribed-caller.c" "$output_dir/undescribed-caller" \
    -O0 -g3 -fno-toplevel-reorder -fno-omit-frame-pointer -fPIE -pie
build_fixture gcc "$c_fixtures_dir/repeated-calls.c" "$output_dir/repeated-calls-gcc-o0" \
    -O0 -g3 -fno-omit-frame-pointer -fPIE -pie
build_fixture clang "$c_fixtures_dir/repeated-calls.c" "$output_dir/repeated-calls-clang-o0" \
    -O0 -g3 -fno-omit-frame-pointer -fPIE -pie
build_fixture gcc "$c_fixtures_dir/freestanding-entry.c" "$output_dir/freestanding-entry" \
    -O0 -g3 -static -nostdlib -fno-pie -no-pie -fno-stack-protector
build_fixture gcc "$c_fixtures_dir/freestanding-entry.c" "$output_dir/freestanding-entry-bare" \
    -O0 -g3 -static -nostdlib -fno-pie -no-pie -fno-stack-protector -DBARE_ENTRY
build_fixture gcc "$c_fixtures_dir/unwind.c" "$output_dir/unwind-o0" \
    -O0 -g3 -fno-omit-frame-pointer -fPIE -pie
build_fixture gcc "$c_fixtures_dir/unwind.c" "$output_dir/unwind-o2" \
    -O2 -g3 -fomit-frame-pointer -fPIE -pie
build_fixture gcc "$c_fixtures_dir/unwind.c" "$output_dir/unwind-nopie" \
    -O2 -g3 -fomit-frame-pointer -no-pie
build_fixture clang "$c_fixtures_dir/unwind.c" "$output_dir/unwind-clang-o2" \
    -O2 -g3 -fomit-frame-pointer -fPIE -pie
# Caller frames whose values live in frame slots, in registers callees saved,
# and in registers no callee saves.
build_fixture gcc "$c_fixtures_dir/frames.c" "$output_dir/frames-gcc-o0" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie
build_fixture gcc "$c_fixtures_dir/frames.c" "$output_dir/frames-gcc-o2" \
    -O2 -g3 -gdwarf-5 -fomit-frame-pointer -fPIE -pie
build_fixture gcc "$c_fixtures_dir/frames.c" "$output_dir/frames-gcc-o2-nopie" \
    -O2 -g3 -gdwarf-5 -fomit-frame-pointer -no-pie
build_fixture clang "$c_fixtures_dir/frames.c" "$output_dir/frames-clang-o0" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie
build_fixture clang "$c_fixtures_dir/frames.c" "$output_dir/frames-clang-o2" \
    -O2 -g3 -gdwarf-5 -fomit-frame-pointer -fPIE -pie
# Values split across registers and memory, recovered from callers' call
# sites, also in another module, and pointing at objects with no address.
build_shared_fixture gcc "$c_fixtures_dir/locations/library.c" "$output_dir/liblocations.so" \
    -O2 -g3 -gdwarf-5 -fomit-frame-pointer
locations_library=("-L$output_dir" -llocations '-Wl,-rpath,$ORIGIN')
build_fixture gcc "$c_fixtures_dir/locations/main.c" "$output_dir/locations-gcc-o2" \
    -O2 -g3 -gdwarf-5 -fomit-frame-pointer -fPIE -pie "${locations_library[@]}"
require_dwarf_operation "$output_dir/locations-gcc-o2" 'DW_OP_piece'
require_dwarf_operation "$output_dir/locations-gcc-o2" 'DW_OP_entry_value'
require_dwarf_operation "$output_dir/locations-gcc-o2" 'DW_OP_GNU_parameter_ref'
require_dwarf_operation "$output_dir/locations-gcc-o2" 'DW_OP_implicit_pointer'
build_fixture gcc "$c_fixtures_dir/locations/main.c" "$output_dir/locations-gcc-o2-nopie" \
    -O2 -g3 -gdwarf-5 -fomit-frame-pointer -no-pie "${locations_library[@]}"
build_fixture clang "$c_fixtures_dir/locations/main.c" "$output_dir/locations-clang-o2" \
    -O2 -g3 -gdwarf-5 -fomit-frame-pointer -fPIE -pie "${locations_library[@]}"
require_dwarf_operation "$output_dir/locations-clang-o2" 'DW_OP_piece'
require_dwarf_operation "$output_dir/locations-clang-o2" 'DW_AT_call_tail_call'
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

for variant in "gcc-o0 gcc -O0 -fno-omit-frame-pointer -fPIE -pie" \
    "clang-o2 clang -O2 -fomit-frame-pointer -fPIE -pie" \
    "gcc-o2-nopie gcc -O2 -fomit-frame-pointer -no-pie"; do
    read -r name compiler flags <<<"$variant"
    # shellcheck disable=SC2086
    build_fixture "$compiler" "$c_fixtures_dir/watch.c" "$output_dir/watch-${name}" \
        -g3 -gdwarf-5 $flags
    require_instruction "$output_dir/watch-${name}" repeated_store 'rep stos'
    require_instruction "$output_dir/watch-${name}" paired_store 'movdqu'
    require_instruction "$output_dir/watch-${name}" failed_exchange 'lock cmpxchg'
done
for variant in "gcc-o0 gcc -O0 -fno-omit-frame-pointer -fPIE -pie" \
    "clang-o0 clang -O0 -fno-omit-frame-pointer -fPIE -pie" \
    "clang-o2 clang -O2 -fomit-frame-pointer -fPIE -pie" \
    "gcc-o2-nopie gcc -O2 -fomit-frame-pointer -no-pie"; do
    read -r name compiler flags <<<"$variant"
    # shellcheck disable=SC2086
    build_fixture "$compiler" "$c_fixtures_dir/hit-counts.c" "$output_dir/hit-counts-${name}" \
        -g3 -gdwarf-5 $flags
done
for fixture in hit-count-threads hit-count-spin hit-count-signals; do
    build_fixture gcc "$c_fixtures_dir/${fixture}.c" "$output_dir/${fixture}" \
        -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie -pthread
done
build_fixture gcc "$c_fixtures_dir/watch-threads.c" "$output_dir/watch-threads" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie -pthread
build_fixture gcc "$c_fixtures_dir/watch-steady.c" "$output_dir/watch-steady" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie -pthread
build_fixture gcc "$c_fixtures_dir/watch-steady.c" "$output_dir/watch-steady-spin" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie -pthread -DSPIN
build_fixture gcc "$c_fixtures_dir/watch-locals.c" "$output_dir/watch-locals-gcc-o0" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie -pthread
build_fixture clang "$c_fixtures_dir/watch-locals.c" "$output_dir/watch-locals-clang-o0" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie -pthread
require_tail_jump "$output_dir/watch-locals-gcc-o0" tail_caller tail_callee
require_tail_jump "$output_dir/watch-locals-clang-o0" tail_caller tail_callee
build_fixture gcc "$c_fixtures_dir/watch-slot-thief.c" "$output_dir/watch-slot-thief-main" \
    -O0 -g3 -gdwarf-5 -fPIE -pie -pthread -DSTOLEN_SLOTS=3
build_fixture gcc "$c_fixtures_dir/watch-slot-thief.c" "$output_dir/watch-slot-thief-worker" \
    -O0 -g3 -gdwarf-5 -fPIE -pie -pthread -DSTOLEN_SLOTS=4 -DSTOLEN_BY_WORKER
build_fixture gcc "$c_fixtures_dir/watch-attach.c" "$output_dir/watch-attach" \
    -O0 -g3 -gdwarf-5 -fPIE -pie -pthread
build_fixture gcc "$c_fixtures_dir/watch-orphaner.c" "$output_dir/watch-orphaner" -O0 -g
build_rust_fixture "$rust_fixtures_dir/watch.rs" "$output_dir/watch-rust-o0" \
    -C opt-level=0 -C force-frame-pointers=yes
build_rust_fixture "$rust_fixtures_dir/watch.rs" "$output_dir/watch-rust-o2" \
    -C opt-level=2 -C force-frame-pointers=no
build_zig_fixture "$zig_fixtures_dir/watch.zig" "$output_dir/watch-zig-o0" \
    -O Debug -fPIE -fno-omit-frame-pointer
build_zig_fixture "$zig_fixtures_dir/watch.zig" "$output_dir/watch-zig-o2" \
    -O ReleaseFast -fPIE -fomit-frame-pointer
build_go_fixture "$go_fixtures_dir/watch" "$output_dir/watch-go-o0" \
    -buildmode=pie "-gcflags=all=-N -l"
build_go_fixture "$go_fixtures_dir/frames" "$output_dir/frames-go-o2" \
    -buildmode=pie

build_shared_fixture gcc "$c_fixtures_dir/crash/library.c" "$output_dir/libcrash.so" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -Wl,--build-id
build_shared_fixture gcc "$c_fixtures_dir/crash/library.c" "$output_dir/libcrash-rebuilt.so" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -Wl,--build-id -DCRASH_REBUILT
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
# Later -C options override the debuginfo=2 default; stripping debug info
# keeps the v0-mangled symbol table.
build_rust_fixture "$rust_fixtures_dir/crash.rs" "$output_dir/crash-rust-nodebug" \
    -C opt-level=0 -C debuginfo=0 -C strip=debuginfo
build_go_fixture "$go_fixtures_dir/preempt" "$output_dir/preempt-go" \
    -buildmode=pie
# Tasks are read through the TLS sequence and load bias, which differ
# between a PIE and `go build`'s default executable.
build_go_fixture "$go_fixtures_dir/workers" "$output_dir/workers-go-o0" \
    -buildmode=pie "-gcflags=all=-N -l"
build_go_fixture "$go_fixtures_dir/workers" "$output_dir/workers-go-o2"
# A program that corrupts one of its parked goroutines, at the addresses its
# DWARF gives, which a position-dependent executable runs at.
build_go_fixture "$go_fixtures_dir/corrupt" "$output_dir/corrupt-go"
build_go_fixture "$go_fixtures_dir/scale" "$output_dir/scale-go"
# A large real program, many packages of the standard library's.
build_go_command cmd/gofmt "$output_dir/gofmt-go-o0" -buildmode=pie "-gcflags=all=-N -l"
build_go_command cmd/gofmt "$output_dir/gofmt-go-o2"
build_go_fixture "$go_fixtures_dir/stacks" "$output_dir/stacks-go-o0" \
    -buildmode=pie "-gcflags=all=-N -l"
build_go_fixture "$go_fixtures_dir/stacks" "$output_dir/stacks-go-o2"
build_go_fixture "$go_fixtures_dir/spin" "$output_dir/spin-go-o0" \
    "-gcflags=all=-N -l"
build_go_fixture "$go_fixtures_dir/spin" "$output_dir/spin-go-o2"
build_go_fixture "$go_fixtures_dir/siblings" "$output_dir/siblings-go-o0" \
    -buildmode=pie "-gcflags=all=-N -l"
build_go_fixture "$go_fixtures_dir/siblings" "$output_dir/siblings-go-o2"
build_go_fixture "$go_fixtures_dir/watched" "$output_dir/watched-go-o0" \
    -buildmode=pie "-gcflags=all=-N -l"
build_go_fixture "$go_fixtures_dir/watched" "$output_dir/watched-go-o2"
# Go calls C, which calls back into Go, with each C compiler.
GO_CGO=1 GO_CC=gcc GO_CFLAGS="-g -O0" build_go_fixture "$go_fixtures_dir/cgo" \
    "$output_dir/cgo-go-gcc" -buildmode=pie "-gcflags=all=-N -l"
GO_CGO=1 GO_CC=clang GO_CFLAGS="-g -O2" build_go_fixture "$go_fixtures_dir/cgo" \
    "$output_dir/cgo-go-clang"
# An HTTP server and its client, and the same built without the paths of
# its sources.
build_go_fixture "$go_fixtures_dir/server" "$output_dir/server-go-o0" \
    -buildmode=pie "-gcflags=all=-N -l"
build_go_fixture "$go_fixtures_dir/server" "$output_dir/server-go-trimpath" -trimpath
# A server to attach to.
build_go_fixture "$go_fixtures_dir/served" "$output_dir/served-go" -buildmode=pie
# A C program that hosts a Go library and calls into its runtime.
GO_CGO=1 GO_CC=gcc GO_CFLAGS="-g -O0" build_go_fixture "$go_fixtures_dir/hosted" \
    "$output_dir/libgo-hosted.so" -buildmode=c-shared "-gcflags=all=-N -l"
build_fixture gcc "$c_fixtures_dir/go-host/main.c" "$output_dir/go-host" \
    -O0 -g3 -gdwarf-5 -fPIE -pie "-L$output_dir" -lgo-hosted '-Wl,-rpath,$ORIGIN'
build_go_fixture "$go_fixtures_dir/torture" "$output_dir/torture-go-o0" \
    -buildmode=pie "-gcflags=all=-N -l"
build_go_fixture "$go_fixtures_dir/torture" "$output_dir/torture-go-o2"
build_go_fixture "$go_fixtures_dir/growing" "$output_dir/growing-go" \
    -buildmode=pie "-gcflags=all=-N -l"
build_go_fixture "$go_fixtures_dir/steps" "$output_dir/steps-go-o0" \
    -buildmode=pie "-gcflags=all=-N -l"
build_go_fixture "$go_fixtures_dir/steps" "$output_dir/steps-go-o2"
build_go_fixture "$go_fixtures_dir/defers" "$output_dir/defers-go-o0" \
    -buildmode=pie "-gcflags=all=-N -l"
build_go_fixture "$go_fixtures_dir/defers" "$output_dir/defers-go-o2"
build_go_fixture "$go_fixtures_dir/failing" "$output_dir/failing-go-o0" \
    -buildmode=pie "-gcflags=all=-N -l"
build_go_fixture "$go_fixtures_dir/failing" "$output_dir/failing-go-o2"
build_go_fixture "$go_fixtures_dir/ranges" "$output_dir/ranges-go-o0" \
    -buildmode=pie "-gcflags=all=-N -l"
build_go_fixture "$go_fixtures_dir/ranges" "$output_dir/ranges-go-o2"
build_go_fixture "$go_fixtures_dir/crash" "$output_dir/crash-go-o0" \
    -buildmode=pie "-gcflags=all=-N -l"
build_go_fixture "$go_fixtures_dir/panic" "$output_dir/panic-go-o0" \
    -buildmode=pie "-gcflags=all=-N -l"
build_go_fixture "$go_fixtures_dir/panic" "$output_dir/panic-go-o2"
build_go_fixture "$go_fixtures_dir/crash" "$output_dir/crash-go-nodwarf" \
    -buildmode=pie "-gcflags=all=-N -l" -ldflags=-w
build_zig_fixture "$zig_fixtures_dir/crash.zig" "$output_dir/crash-zig-o0" \
    -O Debug -fPIE -fno-omit-frame-pointer
# A call chain the program records itself, built as `go build` does by
# default, stripped of DWARF and symbols, and stripped after an external link,
# which puts Go's code after the C runtime's.
build_go_fixture "$go_fixtures_dir/callers" "$output_dir/callers-go"
build_go_fixture "$go_fixtures_dir/callers" "$output_dir/callers-go-stripped" \
    -buildmode=pie "-ldflags=-s -w"
GO_CGO=1 build_go_fixture "$go_fixtures_dir/callers" "$output_dir/callers-go-external-stripped" \
    "-ldflags=-linkmode=external -s -w"
build_fixture gcc "$c_fixtures_dir/vdso.c" "$output_dir/vdso-gcc-o0" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie
build_fixture gcc "$c_fixtures_dir/vdso.c" "$output_dir/vdso-gcc-o2" \
    -O2 -g3 -gdwarf-5 -fomit-frame-pointer -fPIE -pie
build_fixture clang "$c_fixtures_dir/vdso.c" "$output_dir/vdso-clang-o2-nopie" \
    -O2 -g3 -gdwarf-5 -fomit-frame-pointer -no-pie

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
for variant in gcc-o0 gcc-o2 gcc-o2-nopie clang-o0 clang-o2; do
    program="$output_dir/frames-${variant}"
    generate_core "${program}.core" 11 "$default_core_filter" "$program" "$program" crash
done
program="$output_dir/tls-modules-musl-gcc-o0"
generate_core "${program}.core" 6 "$default_core_filter" \
    "$program $output_dir/libtls-modules-musl.so $output_dir/libtls-plugin-musl.so" \
    "$program" abort
for variant in musl-clang-static-pie gcc-static single-thread-clang-static; do
    program="$output_dir/tls-modules-${variant}"
    generate_core "${program}.core" 6 "$default_core_filter" "$program" "$program" abort
done
for variant in gcc-o2 gcc-o2-nopie clang-o2; do
    program="$output_dir/locations-${variant}"
    generate_core "${program}.core" 6 "$default_core_filter" \
        "$program $output_dir/liblocations.so" "$program" abort
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

# A core from another machine. Its program ran beside its own build of the C
# library; afterwards the executable and library at its recorded paths are
# replaced by different builds and the C library is removed, so only a
# sysroot or module path holding the originals can supply them.
toolchain_libc=$(ldd "$output_dir/crash-gcc-o0" | awk '$1 == "libc.so.6" { print $3 }')
derive_foreign_libc "$toolchain_libc" "$output_dir/libc-foreign.so.6"
generate_foreign_core() {
    local directory="$output_dir/core-foreign"
    local core="$directory/crash.core"
    local inputs="$output_dir/crash-gcc-o0 $output_dir/libcrash.so $output_dir/libc-foreign.so.6"
    mkdir -p "$directory"
    if ! core_is_current "$core" "$(core_signature 11 "$default_core_filter" \
        "$inputs" "$directory/crash-gcc-o0" segv)"; then
        cp "$output_dir/crash-gcc-o0" "$output_dir/libcrash.so" "$directory/"
        cp "$output_dir/libc-foreign.so.6" "$directory/libc.so.6"
    fi
    generate_core "$core" 11 "$default_core_filter" "$inputs" "$directory/crash-gcc-o0" segv
    cp "$output_dir/crash-gcc-o0-rebuilt" "$directory/crash-gcc-o0"
    cp "$output_dir/libcrash-rebuilt.so" "$directory/libcrash.so"
    rm -f "$directory/libc.so.6"
}
generate_foreign_core

# Cores of programs whose code ELF symbols alone describe. The Go runtime's
# arenas would make a full core enormous, and frame 0 needs only registers.
generate_core "$output_dir/crash-rust-nodebug.core" 11 "$default_core_filter" \
    "$output_dir/crash-rust-nodebug" "$output_dir/crash-rust-nodebug"
# A Go program that panics with GOTRACEBACK=crash prints every goroutine,
# which the core's log keeps, and aborts.
for variant in o0 o2; do
    program="$output_dir/panic-go-${variant}"
    generate_core "${program}.core" 6 "$default_core_filter" "$program" \
        env GOTRACEBACK=crash "$program"
done
generate_core "$output_dir/crash-go-nodwarf.core" 11 "$headers_only_core_filter" \
    "$output_dir/crash-go-nodwarf" "$output_dir/crash-go-nodwarf"

generate_core "$output_dir/null-call.core" 11 "$default_core_filter" \
    "$output_dir/null-call" "$output_dir/null-call"

# Cores of programs that faulted inside the vDSO, which no file backs.
readonly vdso_variants=(gcc-o0 gcc-o2 clang-o2-nopie)
for variant in "${vdso_variants[@]}"; do
    program="$output_dir/vdso-${variant}"
    for mode in clock time; do
        generate_core "${program}-${mode}.core" 11 "$default_core_filter" "$program" \
            "$program" "$mode"
    done
done

# Independent readings of the ELF symbol fixture by binutils and gdb, which
# differential tests compare against uscope's symbol tables and backtraces.
readonly symbol_oracle_dir="$output_dir/symbol-oracles"
mkdir -p "$symbol_oracle_dir"

# Records readelf's section and symbol tables and call-frame entries for one
# ELF file, and the symbols objdump synthesizes for its PLT stubs. An
# embedded MiniDebugInfo object has no frame contents to dump.
generate_symbol_oracle() {
    local elf="$1"
    local oracle="$symbol_oracle_dir/${2:-${elf##*/}}.readelf"
    local frames="${3:-yes}"
    # Nix store files all date from 1970, so the modification time alone never
    # notices a toolchain update. The resolved path names the store entry.
    local header
    header="uscope-symbol-oracle-v4 $(readlink -f "$elf")"
    # Registered so that deleting an oracle invalidates the cached suite.
    rebuilt_outputs["$oracle"]=false
    if [[ -s "$oracle" && "$oracle" -nt "$elf" && "$(head -n 1 "$oracle")" == "$header" ]]; then
        printf '[cached] %s\n' "$oracle"
        return
    fi
    printf '[oracle] %s\n' "$oracle"
    {
        printf '%s\n' "$header"
        readelf -SW "$elf"
        readelf -sW "$elf"
        if [[ "$frames" == yes ]]; then
            readelf -wf "$elf"
            objdump -d -j .plt -j .plt.sec -j .plt.got "$elf" 2>/dev/null | grep '@plt>:$' || true
        fi
    } >"${oracle}.tmp"
    mv "${oracle}.tmp" "$oracle"
}

# Records gdb's complete backtrace of a core's crashing thread.
generate_backtrace_oracle() {
    local program="$1"
    local core="$2"
    local oracle="${core}.gdb-backtrace"
    rebuilt_outputs["$oracle"]=false
    if [[ -s "$oracle" && "$oracle" -nt "$core" ]]; then
        printf '[cached] %s\n' "$oracle"
        return
    fi
    printf '[oracle] %s\n' "$oracle"
    gdb -nx -batch -q \
        -iex 'set auto-load off' \
        -iex 'set debuginfod enabled off' \
        -ex 'set backtrace past-main on' \
        -ex 'set backtrace past-entry on' \
        -ex 'bt' \
        "$program" "$core" 2>&1 \
        | awk '/^#0 / { count = 0 } /^#/ { lines[count++] = $0 }
               END { for (i = 0; i < count; i++) print lines[i] }' >"${oracle}.tmp"
    mv "${oracle}.tmp" "$oracle"
}

# Records gdb's variables for every frame of every thread in a core.
generate_frame_oracle() {
    local program="$1"
    local core="$2"
    local oracle="${core}.gdb-frame-variables"
    rebuilt_outputs["$oracle"]=false
    if [[ -s "$oracle" && "$oracle" -nt "$core" && "$oracle" -nt "$frame_oracle_script" ]]; then
        printf '[cached] %s\n' "$oracle"
        return
    fi
    printf '[oracle] %s\n' "$oracle"
    local log
    if ! log=$(USCOPE_FRAME_ORACLE="${oracle}.tmp" gdb -nx -batch -q \
        -iex 'set auto-load off' \
        -iex 'set debuginfod enabled off' \
        -x "$frame_oracle_script" \
        "$program" "$core" 2>&1) || [[ ! -s "${oracle}.tmp" ]]; then
        printf 'error: gdb did not record frame variables for %s:\n%s\n' "$core" "$log" >&2
        rm -f "${oracle}.tmp"
        exit 1
    fi
    mv "${oracle}.tmp" "$oracle"
}

for variant in gcc-o0 gcc-o2 gcc-o2-nopie clang-o0 clang-o2; do
    generate_frame_oracle "$output_dir/frames-${variant}" "$output_dir/frames-${variant}.core"
done
for variant in gcc-o2 gcc-o2-nopie clang-o2; do
    generate_frame_oracle "$output_dir/locations-${variant}" \
        "$output_dir/locations-${variant}.core"
done
for variant in gcc-o0 clang-o2 gcc-o2-nopie; do
    for kind in segv abort; do
        generate_frame_oracle "$output_dir/crash-${variant}" \
            "$output_dir/crash-${variant}-${kind}.core"
    done
done
for language in rust go zig; do
    generate_frame_oracle "$output_dir/crash-${language}-o0" "$output_dir/crash-${language}-o0.core"
done
for variant in "${vdso_variants[@]}"; do
    for mode in clock time; do
        program="$output_dir/vdso-${variant}"
        generate_backtrace_oracle "$program" "${program}-${mode}.core"
        generate_frame_oracle "$program" "${program}-${mode}.core"
    done
done

for library in gcc clang stripped minidebug; do
    generate_symbol_oracle "$output_dir/libelf-symbols-${library}.so"
done
generate_symbol_oracle "$output_dir/libelf-symbols-minidebug.so.embedded" \
    libelf-symbols-minidebug.so.embedded no
# The unstripped build locates code whose symbols stripping removed.
generate_symbol_oracle "$output_dir/libelf-symbols-stripped.so.full"
for variant in "${symbols_variants[@]}"; do
    read -r name _ <<<"$variant"
    generate_symbol_oracle "$output_dir/elf-symbols-${name}"
done
# The C library and loader every fixture runs against, as the loader resolves them.
while read -r library; do
    generate_symbol_oracle "$library"
done < <(ldd "$output_dir/elf-symbols-gcc-o0" | awk '/=> \// { print $3 } /^\t\// { print $1 }' \
    | grep -E '/(libc\.so|ld-linux)')
for variant in "${symbols_variants[@]}"; do
    read -r name library _ <<<"$variant"
    program="$output_dir/elf-symbols-${name}"
    generate_core "${program}.core" 4 "$default_core_filter" \
        "$program $output_dir/libelf-symbols-${library}.so" "$program"
    generate_backtrace_oracle "$program" "${program}.core"
done

# gdb's type and target function of each function pointer, one per line as
# `name<TAB>type<TAB>function`, read from the executable's own data.
generate_function_type_oracle() {
    local program="$1"
    local oracle="${program}.gdb-function-types"
    rebuilt_outputs["$oracle"]=false
    if [[ -s "$oracle" && "$oracle" -nt "$program" ]]; then
        printf '[cached] %s\n' "$oracle"
        return
    fi
    printf '[oracle] %s\n' "$oracle"
    local -a names=(unary unary_pointer operation 'operations[0]' no_arguments with_variadic
        chooser unprototyped handlers.on_event handlers.on_done namer constant_function
        null_function)
    local name
    for name in "${names[@]}"; do
        gdb -nx -batch -q -iex 'set auto-load off' -iex 'set debuginfod enabled off' \
            -ex "whatis $name" -ex "print $name" "$program" 2>&1 \
            | awk -v name="$name" '
                /^type = / { type = substr($0, 8) }
                /^\$1 = / { target = ""; if (match($0, /<[^>]*>$/)) {
                    target = substr($0, RSTART + 1, RLENGTH - 2) } }
                END { printf "%s\t%s\t%s\n", name, type, target }'
    done >"${oracle}.tmp"
    mv "${oracle}.tmp" "$oracle"
}
for compiler in gcc clang; do
    generate_function_type_oracle "$output_dir/function-types-${compiler}-o0"
done

# The tokio fixtures: one cargo workspace whose crates come only from its
# lockfile, which flake.nix vendors, so building fetches nothing. Each
# variant has a target directory of its own, and cargo rebuilds only what
# changed; a binary is copied out only when cargo rewrote it.
readonly tokio_fixtures_dir="${rust_fixtures_dir}/tokio"
readonly tokio_target_dir="build/tokio-target"
if [[ -z "${USCOPE_FIXTURE_CRATES-}" ]]; then
    printf 'error: USCOPE_FIXTURE_CRATES is unset; build inside the Nix shell\n' >&2
    exit 1
fi

# Builds the workspace's PACKAGES with PROFILE and extra RUSTFLAGS, and
# copies each binary to tokio-NAME-VARIANT.
build_tokio_variant() {
    local variant="$1"
    local profile="$2"
    local flags="$3"
    shift 3
    local -a packages=()
    local package
    for package in "$@"; do
        packages+=(--package "$package")
    done
    local target="$tokio_target_dir/$variant"
    printf '[cargo]  tokio fixtures (%s)\n' "$variant"
    CARGO_TARGET_DIR="$target" RUSTFLAGS="${RUSTFLAGS-} -D warnings ${flags}" \
        NIX_HARDENING_ENABLE= cargo build --quiet --offline --locked \
        --manifest-path "$tokio_fixtures_dir/Cargo.toml" --profile "$profile" \
        --config "source.crates-io.replace-with='vendored'" \
        --config "source.vendored.directory='${USCOPE_FIXTURE_CRATES}'" "${packages[@]}"
    local directory="$profile"
    [[ "$profile" == dev ]] && directory=debug
    for package in "$@"; do
        local built="$target/$directory/$package"
        local output="$output_dir/tokio-${package}-${variant}"
        if [[ -x "$output" ]] && ! [[ "$built" -nt "$output" ]]; then
            rebuilt_outputs["$output"]=false
            continue
        fi
        cp -p "$built" "$output"
        rebuilt_outputs["$output"]=true
    done
    for package in "$@"; do
        generate_coroutine_oracle "$output_dir/tokio-${package}-${variant}"
    done
}

# readelf's description of each coroutine a program has, which a test
# compares uscope's reading of them with.
generate_coroutine_oracle() {
    local program="$1"
    local oracle="${program}.coroutines"
    local reducer=scripts/coroutine-oracle.awk
    if [[ -s "$oracle" && "$oracle" -nt "$program" && "$oracle" -nt "$reducer" ]]; then
        printf '[cached] %s\n' "$oracle"
        rebuilt_outputs["$oracle"]=false
        return
    fi
    printf '[oracle] %s\n' "$oracle"
    rebuilt_outputs["$oracle"]=true
    local dump="${oracle}.info"
    readelf --debug-dump=info "$program" >"$dump" 2>/dev/null
    awk -f "$reducer" "$dump" "$dump" | LC_ALL=C sort -u >"${oracle}.tmp"
    rm -f "$dump"
    mv "${oracle}.tmp" "$oracle"
}

# Every fixture, unoptimized and optimized.
readonly tokio_fixtures=(std-async panics)
build_tokio_variant o0 dev "" "${tokio_fixtures[@]}"
build_tokio_variant o3 release "" "${tokio_fixtures[@]}"
# Panics that abort rather than unwind.
build_tokio_variant abort abort "" panics

# Go's own reading of the function tables of images the Go linker linked,
# which a test compares uscope's reader with.
readonly gosym_oracle="$output_dir/gosym-oracle"
build_go_fixture scripts/gosym-oracle "$gosym_oracle"
generate_gosym_oracle() {
    local program="$1"
    local oracle="${program}.gosym"
    rebuilt_outputs["$oracle"]=false
    if [[ -s "$oracle" && "$oracle" -nt "$program" && "$oracle" -nt "$gosym_oracle" ]]; then
        printf '[cached] %s\n' "$oracle"
        return
    fi
    printf '[oracle] %s\n' "$oracle"
    "$gosym_oracle" "$program" >"${oracle}.tmp"
    mv "${oracle}.tmp" "$oracle"
}
generate_gosym_oracle "$output_dir/callers-go"
generate_gosym_oracle "$output_dir/callers-go-stripped"

# GNU objdump's decoding of every executable section, which differential tests
# compare against uscope's disassembly. -z keeps the zero-filled runs objdump
# otherwise elides.
readonly disassembly_oracle_dir="$output_dir/disassembly-oracles"
mkdir -p "$disassembly_oracle_dir"

generate_disassembly_oracle() {
    local elf="$1"
    local oracle="$disassembly_oracle_dir/${elf##*/}.objdump"
    local header
    header="uscope-disassembly-oracle-v1 $(readlink -f "$elf")"
    rebuilt_outputs["$oracle"]=false
    if [[ -s "$oracle" && "$oracle" -nt "$elf" && "$(head -n 1 "$oracle")" == "$header" ]]; then
        printf '[cached] %s\n' "$oracle"
        return
    fi
    printf '[oracle] %s\n' "$oracle"
    {
        printf '%s\n' "$header"
        objdump -d -z -w "$elf"
    } >"${oracle}.tmp"
    mv "${oracle}.tmp" "$oracle"
}

for program in crash-gcc-o0 crash-gcc-o2-nopie crash-clang-o2 crash-rust-o0 crash-zig-o0 \
    crash-go-o0 libcrash.so elf-symbols-gcc-o0 libelf-symbols-gcc.so \
    libelf-symbols-stripped.so; do
    generate_disassembly_oracle "$output_dir/$program"
done
while read -r library; do
    generate_disassembly_oracle "$library"
done < <(ldd "$output_dir/crash-gcc-o0" | awk '/=> \// { print $3 } /^\t\// { print $1 }' \
    | grep -E '/(libc\.so|ld-linux)')

printf '%s\n' "${!rebuilt_outputs[@]}" >"${suite_outputs}.tmp"
mv "${suite_outputs}.tmp" "$suite_outputs"
mv "${suite_stamp}.tmp" "$suite_stamp"
