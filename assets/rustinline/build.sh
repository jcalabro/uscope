#!/usr/bin/env bash

set -x
rustc -o out -C opt-level=0 -C debuginfo=2 main.rs
