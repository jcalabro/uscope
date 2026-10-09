#!/usr/bin/env bash
# Compares this checkout's answers with the reference binary's: dumps each
# program with both, in parallel, and shows where the dumps differ. With no
# programs, it compares the differential corpus. Pass `--sections a,b` (or
# any other dump option) after `--` to narrow every dump.
#
#   scripts/differential.sh [PROGRAM...] [-- DUMP-OPTION...]

set -euo pipefail

reference=target/reference/uscope-tools
current=target/debug/uscope-tools
out=target/differential
[[ -x "$reference" ]] || { echo "error: no reference binary; run \`just reference\`" >&2; exit 2; }

programs=()
options=()
while (( $# > 0 )); do
    if [[ "$1" == -- ]]; then
        shift
        options=("$@")
        break
    fi
    programs+=("$1")
    shift
done
if (( ${#programs[@]} == 0 )); then
    programs=(
        build/golden/straight/straight-gcc-O2
        build/golden/containers/containers-clang-O0-pie
        build/test-programs/basic
        build/test-programs/containers-cpp-gcc-o0
        build/test-programs/containers-cpp-gcc-o2
        build/test-programs/containers-cpp-clang-o0
        build/test-programs/containers-cpp-clang-o2
        build/test-programs/containers-cpp-libcxx-o2
        build/test-programs/containers-rust-o0
        build/test-programs/containers-rust-o2
        build/test-programs/containers-zig-o0
        build/test-programs/containers-zig-self-hosted
        build/test-programs/containers-go-o0
        build/test-programs/containers-go-o2
        build/test-programs/callers-go-stripped
        build/test-programs/cgo-go-gcc
        build/test-programs/tokio-workers-o3
    )
fi

mkdir -p "$out"
status=0
for program in "${programs[@]}"; do
    name="$(basename "$program")"
    "$reference" dump "$program" "${options[@]}" -o "$out/$name.reference" &
    "$current" dump "$program" "${options[@]}" -o "$out/$name.current" &
    wait %1 %2
    if cmp -s "$out/$name.reference" "$out/$name.current"; then
        echo "same      $name"
        rm -f "$out/$name.reference" "$out/$name.current"
    else
        echo "DIFFERENT $name: diff $out/$name.reference $out/$name.current"
        diff "$out/$name.reference" "$out/$name.current" | head -20 | cut -c1-400
        status=1
    fi
done
exit "$status"
