#!/usr/bin/env bash

set -x
${CC:-clang} -Wall -Wextra -Werror -no-pie -O0 -g -gdwarf-${DWARF:-5} -o out main.c
