#!/usr/bin/env bash

set -x

COMPILE="${CC:-clang} -Wall -Wextra -Werror -fPIC -O0 -g -gdwarf-${DWARF:-5}"

$($COMPILE -o sample.o -c lib.c)
$($COMPILE -o libsample.so -shared sample.o)
$($COMPILE -o out -L $(pwd) -Wl,-rpath=$(pwd) -lsample main.c)
