#!/usr/bin/env bash

set -x
${CC:-clang} -Wall -Wextra -Werror -no-pie -O0 -g -o out main.c
