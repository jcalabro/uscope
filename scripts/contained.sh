#!/usr/bin/env bash
# Runs a command so that it cannot exhaust the machine's memory: in its own
# systemd scope capped at half the memory (or USCOPE_MEMORY_MAX), without
# swap, and first in line for the OOM killer, so a runaway job dies instead
# of the desktop session.

set -euo pipefail

[[ $# -gt 0 ]] || { echo "usage: contained.sh COMMAND [ARGUMENT...]" >&2; exit 2; }

memory_max="${USCOPE_MEMORY_MAX:-50%}"
echo 1000 >/proc/self/oom_score_adj
exec systemd-run --user --scope --quiet \
    -p MemoryMax="$memory_max" -p MemorySwapMax=0 -p OOMPolicy=continue \
    -- "$@"
