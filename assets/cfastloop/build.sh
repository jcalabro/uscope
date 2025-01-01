#!/usr/bin/env bash

set -x
${CC:-gcc} -Wall -Wextra -Werror -no-pie -O0 -g -gdwarf-${DWARF:-5} -o out -fPIE main.c
