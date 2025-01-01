#!/usr/bin/env bash

ITERATIONS=${1:-100}
echo "running tests on the existing binary $ITERATIONS times"
for I in $( seq 0 $ITERATIONS); do
  ./zig-out/bin/panacea-tests || exit 1
done
