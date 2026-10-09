#!/usr/bin/env bash
# Fails unless the Tracy client that uscope's `tracy` feature builds is the
# version of the viewer `nix develop .#profile` provides: a Tracy viewer reads
# only what a client of its own release sends.
set -euo pipefail

: "${TRACY_VERSION:?run this in nix develop .#profile, which provides the viewer}"
manifest="$(cargo metadata --format-version 1 --features tracy |
    grep -o '"manifest_path":"[^"]*/tracy-client-sys-[^"/]*/Cargo.toml"' |
    head -n 1 | cut -d '"' -f 4)"
[[ -n "$manifest" ]] || { echo "tracy-client-sys is not locked" >&2; exit 1; }
header="$(dirname "$manifest")/tracy/common/TracyVersion.hpp"
client="$(sed -n 's/.*\(Major\|Minor\|Patch\) = \([0-9]*\).*/\2/p' "$header" | paste -sd .)"
if [[ "$client" != "$TRACY_VERSION" ]]; then
    echo "the Tracy client is $client but the viewer is $TRACY_VERSION; pin them together" >&2
    exit 1
fi
echo "the Tracy client and viewer are both $client"
