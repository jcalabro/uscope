#!/usr/bin/env bash

DIRS=""
if [ -z "$1" ]; then
    # no argument provded; rebuild all
    DIRS=$(ls | grep -v .sh | grep -v test_files)
    echo Building all assets
elif [ "$1" == "CI" ]; then
    # CI doesn't have access to all compilers
    DIRS=$(ls | grep -v .sh | grep -v test_files | grep -v jai)
    echo Building all assets
else
    DIRS=$1
fi

for DIR in $DIRS; do
    if [ ! -d "$DIR" ]; then
        echo "Directory not found: $DIR"
        exit 1
    fi

    echo "Building $DIR"
    pushd $DIR > /dev/null

    ./build.sh

    popd > /dev/null
    echo ''
done
