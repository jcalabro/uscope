#!/usr/bin/env bash

set -x
cargo build
cp target/debug/rustloop out
