#!/usr/bin/env bash

#
# @NOTE (jrc): this script uses gcc, but forces DWARF v5 for variety
#

set -x
${CC:-gcc} -Wall -Wextra -Werror -no-pie -O0 -g -gdwarf-${DWARF:-5} -o out main.c
