#!/usr/bin/env bash
# Runs commands at once and fails if any fails. Each argument is a NAME=COMMAND
# pair, run with bash. The first command's output streams as it runs; each
# other's is held and printed whole, if it printed anything, when it
# finishes, so short jobs read as one block among a long one's lines. A
# summary names each command's result and time.
#
#   concurrently.sh 'test=just test' 'lint=just lint'

set -euo pipefail

[[ $# -gt 0 ]] || { echo "usage: concurrently.sh NAME=COMMAND..." >&2; exit 2; }

held=$(mktemp -d)
pids=()
trap 'kill "${pids[@]}" 2>/dev/null || true; rm -r -- "$held"' EXIT

run() {
    local index=$1 name=$2 command=$3 start=$EPOCHREALTIME status=0
    if (( index == 0 )); then
        bash -c "$command" || status=$?
    else
        bash -c "$command" >"$held/$index.out" 2>&1 || status=$?
        if [[ -s "$held/$index.out" ]]; then
            { printf '──── %s ────\n' "$name"; cat "$held/$index.out"; }
        fi
    fi
    printf '%s %s %.1f\n' "$status" "$name" "$(bc <<<"$EPOCHREALTIME - $start")" >"$held/$index.status"
    return "$status"
}

index=0
for job in "$@"; do
    run "$index" "${job%%=*}" "${job#*=}" &
    pids+=($!)
    index=$(( index + 1 ))
done

failed=0
for pid in "${pids[@]}"; do
    wait "$pid" || failed=1
done
pids=()

for (( index = 0; index < $#; index++ )); do
    read -r status name seconds <"$held/$index.status"
    if (( status == 0 )); then
        printf '  ok      %-16s %6ss\n' "$name" "$seconds"
    else
        printf '  FAILED  %-16s %6ss (exit %s)\n' "$name" "$seconds" "$status"
    fi
done
exit "$failed"
