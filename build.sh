#!/bin/bash

# fail on error
set -e

git submodule init
git submodule update

BUILD_DIR=./build
mkdir -p "$BUILD_DIR"
cd "$BUILD_DIR"

[ ! -f Makefile ] && cmake ..
make
