#!/usr/bin/env bash
# Runs a command in the pinned Nix development shell, entering it unless
# this already runs there, so that nothing a recipe runs can find the host's
# tools or settings: the justfile runs every recipe through this.
set -euo pipefail

if [[ "${USCOPE_HERMETIC:-}" == 1 ]]; then
    exec "$@"
fi
exec "$(dirname "${BASH_SOURCE[0]}")/dev.sh" --command "$@"
