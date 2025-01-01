#!/usr/bin/env bash

DIRS=""
if [ -z "$1" ]; then
    # no argument provded; clean all
    DIRS=$(ls | grep -v .sh | grep -v test_files)
else
    DIRS=$1
fi

for DIR in $DIRS; do
    echo "Cleaning $DIR"
    pushd $DIR > /dev/null

    ./clean.sh

    popd > /dev/null
    echo ''
done
