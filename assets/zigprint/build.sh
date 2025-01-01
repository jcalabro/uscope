#!/usr/bin/env bash

set -x
zigup run $(cat ../../zig_version.txt) build-exe -femit-bin=out main.zig
rm -f out.o
