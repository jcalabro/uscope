#!/usr/bin/env bash
# Builds and checks the simulator's golden programs, which are checked in as
# source and as compiled binaries so that compiler upgrades never change what
# a simulation means.
#
#   golden.sh build NAME   compiles NAME's variants, runs each with every
#                          argument list in NAME/arguments, and rewrites
#                          NAME/manifest.json and NAME/facts.json
#   golden.sh check        fails unless every manifest matches its sources,
#                          binaries, and the output the binaries produce, and
#                          every facts file matches its binaries

set -euo pipefail

readonly golden_dir="tests/golden"
# Debug information places the repository here wherever it is checked out.
readonly source_root="/uscope"
# How many more times a build runs each binary to check that it behaves the
# same however its threads interleave.
readonly determinism_runs=20
readonly common_flags="-std=c11 -g -nostdlib -ffreestanding -fno-builtin -fno-stack-protector -fcf-protection=none -Wall -Wextra -Werror"
readonly static="-static -fno-pie"
readonly static_pie="-static-pie -fpie"
# Vectorized loops would need SSE arithmetic, which the simulator does not
# model. The toolchain keeps frame pointers unless told otherwise, so every
# variant says which it wants.
readonly gcc_O2="-O2 -fno-tree-vectorize"
readonly clang_O2="-O2 -fno-vectorize -fno-slp-vectorize"
readonly variants=(
    "gcc-O0 gcc -O0 -fno-omit-frame-pointer ${static}"
    "gcc-O0-nofp gcc -O0 -fomit-frame-pointer ${static}"
    "gcc-O2 gcc ${gcc_O2} -fno-omit-frame-pointer ${static}"
    "gcc-O2-nofp gcc ${gcc_O2} -fomit-frame-pointer ${static}"
    "gcc-O2-pie gcc ${gcc_O2} -fomit-frame-pointer ${static_pie}"
    "clang-O0 clang -O0 -fno-omit-frame-pointer ${static}"
    "clang-O0-nofp clang -O0 -fomit-frame-pointer ${static}"
    "clang-O0-pie clang -O0 -fno-omit-frame-pointer ${static_pie}"
    "clang-O2 clang ${clang_O2} -fno-omit-frame-pointer ${static}"
    "clang-O2-nofp clang ${clang_O2} -fomit-frame-pointer ${static}"
)

die() {
    printf 'golden: %s\n' "$*" >&2
    exit 1
}

json_string() {
    local text="$1"
    text=${text//\\/\\\\}
    text=${text//\"/\\\"}
    text=${text//$'\n'/\\n}
    text=${text//$'\t'/\\t}
    [[ "$text" != *[[:cntrl:]]* ]] || die "unsupported control character in ${1@Q}"
    printf '"%s"' "$text"
}

hash_of() {
    sha256sum "$1" | cut -d' ' -f1
}

# The sources a program's binaries are built from, relative to its directory.
# Programs that include the thread runtime link it too.
sources_of() {
    local name="$1"
    printf '%s\n' "${name}.c" ../rt/rt.c ../rt/rt.h
    if grep -q '^#include "../rt/thread.h"$' "${golden_dir}/${name}/${name}.c"; then
        printf '%s\n' ../rt/thread.c ../rt/thread.h
    fi
}

# Prints the manifest the files of program NAME describe now, recording the
# given compiler versions.
manifest() {
    local name="$1" gcc_version="$2" clang_version="$3"
    local directory="${golden_dir}/${name}"
    local separator

    printf '{\n  "program": %s,\n' "$(json_string "$name")"
    printf '  "toolchain": {\n    "gcc": %s,\n    "clang": %s\n  },\n' \
        "$(json_string "$gcc_version")" "$(json_string "$clang_version")"

    printf '  "sources": [\n'
    separator=""
    while read -r source; do
        printf '%s    {"path": %s, "sha256": "%s"}' "$separator" \
            "$(json_string "$source")" "$(hash_of "${directory}/${source}")"
        separator=$',\n'
    done < <(sources_of "$name")
    printf '\n  ],\n'

    printf '  "variants": [\n'
    separator=""
    local variant
    for variant in "${variants[@]}"; do
        read -r variant_name compiler flags <<<"$variant"
        local binary="${directory}/${name}-${variant_name}"
        [[ -f "$binary" ]] || die "${binary} is missing; run: just golden-build ${name}"
        printf '%s    {"name": %s, "compiler": %s, "flags": %s, "sha256": "%s"}' "$separator" \
            "$(json_string "$variant_name")" "$(json_string "$compiler")" \
            "$(json_string "${common_flags} ${flags}")" "$(hash_of "$binary")"
        separator=$',\n'
    done
    printf '\n  ],\n'

    printf '  "runs": [\n'
    separator=""
    local arguments
    while IFS= read -r arguments; do
        local -a argv=()
        read -r -a argv <<<"$arguments"
        local expected_output="" expected_status=""
        for variant in "${variants[@]}"; do
            read -r variant_name _ <<<"$variant"
            local binary="${directory}/${name}-${variant_name}" output status=0
            output=$("./${binary}" "${argv[@]}"; printf x) || status=$?
            if [[ $status -eq 0 ]]; then
                # The command substitution's status is the program's.
                "./${binary}" "${argv[@]}" >/dev/null || status=$?
            fi
            output=${output%x}
            if [[ -z "$expected_status" ]]; then
                expected_output=$output
                expected_status=$status
            elif [[ "$output" != "$expected_output" || "$status" != "$expected_status" ]]; then
                die "${binary} ${arguments} disagrees with the other variants"
            fi
        done
        local json_arguments="" argument_separator=""
        local argument
        for argument in "${argv[@]}"; do
            json_arguments+="${argument_separator}$(json_string "$argument")"
            argument_separator=", "
        done
        printf '%s    {"arguments": [%s], "exit_code": %s, "output": %s}' "$separator" \
            "$json_arguments" "$expected_status" "$(json_string "$expected_output")"
        separator=$',\n'
    done <"${directory}/arguments"
    printf '\n  ]\n}\n'
}

# Prints what GNU binutils, not uscope, say about each variant of program
# NAME: its functions from the symbol table, its line table rows in program
# order, and how many inlined calls its debug information describes. The
# simulator's semantic oracles judge the debugger by these. A row is
# [address, file, line, statement]; line 0 names no source, and -1 ends a
# sequence.
facts() {
    local name="$1"
    local directory="${golden_dir}/${name}"
    local separator="" variant
    printf '{\n  "program": %s,\n  "variants": [\n' "$(json_string "$name")"
    for variant in "${variants[@]}"; do
        read -r variant_name _ flags <<<"$variant"
        local binary="${directory}/${name}-${variant_name}"
        [[ -f "$binary" ]] || die "${binary} is missing; run: just golden-build ${name}"
        local optimized=false
        [[ " ${flags} " == *" -O0 "* ]] || optimized=true
        local inlined
        inlined=$(readelf -W --debug-dump=info "$binary" | grep -c 'DW_TAG_inlined_subroutine' || true)
        printf '%s    {\n      "name": %s,\n      "optimized": %s,\n      "inlined": %s,\n' \
            "$separator" "$(json_string "$variant_name")" "$optimized" "$inlined"
        printf '      "functions": [\n'
        nm --defined-only --print-size "$binary" |
            awk '$3 == "T" || $3 == "t" {
                    printf "%s        [\"%s\", %d, %d]", separator, $4, strtonum("0x" $1), strtonum("0x" $2)
                    separator = ",\n"
                 }
                 END { printf "\n" }'
        printf '      ],\n      "lines": [\n'
        readelf -W --debug-dump=decodedline "$binary" |
            awk '$3 ~ /^0x[0-9a-f]+$/ && ($2 == "-" || $2 ~ /^[0-9]+$/) {
                    file = $1
                    sub(/.*\//, "", file)
                    line = $2 == "-" ? -1 : $2
                    statement = $NF == "x" ? "true" : "false"
                    printf "%s        [%d, \"%s\", %d, %s]", separator, strtonum($3), file, line, statement
                    separator = ",\n"
                 }
                 END { printf "\n" }'
        printf '      ],\n      "epilogues": ['
        # The row each "Set epilogue_begin" precedes: the next one a special
        # opcode or Copy emits.
        readelf -W --debug-dump=rawline "$binary" |
            awk 'match($0, /(set Address|advance Address by [0-9]+|Advance PC by [a-z ]*[0-9]+) to (0x[0-9a-f]+)/, found) {
                    address = strtonum(gensub(/.* to (0x[0-9a-f]+).*/, "\\1", 1, found[0]))
                 }
                 /Set epilogue_begin to true/ { pending = 1; next }
                 pending && (/Special opcode/ || /\]  Copy/) {
                    printf "%s%d", separator, address
                    separator = ", "
                    pending = 0
                 }'
        printf ']\n    }'
        separator=$',\n'
    done
    printf '\n  ]\n}\n'
}

build() {
    local name="$1"
    local directory="${golden_dir}/${name}"
    [[ -f "${directory}/${name}.c" ]] || die "no program ${directory}/${name}.c"
    local variant
    for variant in "${variants[@]}"; do
        read -r variant_name compiler flags <<<"$variant"
        local binary="${directory}/${name}-${variant_name}"
        local -a sources=()
        mapfile -t sources < <(sources_of "$name" | grep '\.c$' | sed "s|^|${directory}/|")
        printf '[build] %s\n' "$binary"
        # shellcheck disable=SC2086
        NIX_HARDENING_ENABLE= "$compiler" $common_flags $flags \
            "-ffile-prefix-map=${PWD}=${source_root}" \
            -o "$binary" "${sources[@]}"
    done
    manifest "$name" "$(gcc --version | head -n1)" "$(clang --version | head -n1)" \
        >"${directory}/manifest.json.tmp"
    mv "${directory}/manifest.json.tmp" "${directory}/manifest.json"
    printf '[wrote] %s/manifest.json\n' "$directory"
    facts "$name" >"${directory}/facts.json.tmp"
    mv "${directory}/facts.json.tmp" "${directory}/facts.json"
    printf '[wrote] %s/facts.json\n' "$directory"
    # Threads interleave differently on every run; what a program prints and
    # returns must not depend on how.
    local run
    for run in $(seq "$determinism_runs"); do
        manifest "$name" "$(gcc --version | head -n1)" "$(clang --version | head -n1)" |
            cmp -s - "${directory}/manifest.json" ||
            die "${name} behaved differently on run ${run}: its output depends on scheduling"
    done
    printf '[checked] %s behaves the same in %s more runs\n' "$name" "$determinism_runs"
}

check() {
    local manifest_path failed=0
    for manifest_path in "${golden_dir}"/*/manifest.json; do
        local name gcc_version clang_version
        name=$(basename "$(dirname "$manifest_path")")
        gcc_version=$(sed -n 's/^    "gcc": "\(.*\)",$/\1/p' "$manifest_path")
        clang_version=$(sed -n 's/^    "clang": "\(.*\)"$/\1/p' "$manifest_path")
        if ! diff -u "$manifest_path" <(manifest "$name" "$gcc_version" "$clang_version"); then
            printf 'golden: %s does not match its files; rebuild with: just golden-build %s\n' \
                "$manifest_path" "$name" >&2
            failed=1
        fi
        local facts_path="${golden_dir}/${name}/facts.json"
        if ! diff -u "$facts_path" <(facts "$name"); then
            printf 'golden: %s does not match its binaries; rebuild with: just golden-build %s\n' \
                "$facts_path" "$name" >&2
            failed=1
        fi
    done
    return "$failed"
}

case "${1:-}" in
build)
    [[ $# -eq 2 ]] || die "usage: golden.sh build NAME"
    build "$2"
    ;;
check)
    [[ $# -eq 1 ]] || die "usage: golden.sh check"
    check
    ;;
*)
    die "usage: golden.sh build NAME | golden.sh check"
    ;;
esac
