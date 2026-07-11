#!/usr/bin/env bash
set -euo pipefail

exec nix --extra-experimental-features 'nix-command flakes' develop "$@"
