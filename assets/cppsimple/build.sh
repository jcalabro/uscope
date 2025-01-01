#!/usr/bin/env bash

set -x
${CXX:-g++} -Wall -Wextra -Werror -no-pie -O0 -g -o out main.cpp
