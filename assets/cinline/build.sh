#!/usr/bin/env bash

set -x
${CC:-gcc} -Wall -Wextra -Werror -no-pie -O2 -g -o out main.c
