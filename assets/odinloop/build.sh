#!/usr/bin/env bash

set -x
odin build . -debug -out:out -vet -strict-style
