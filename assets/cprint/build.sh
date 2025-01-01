#!/usr/bin/env bash

set -x
${CC:-gcc} -Wall -Wextra -Werror -no-pie -Wall -Werror -no-pie -Wextra -O0 -g -o out main.c
