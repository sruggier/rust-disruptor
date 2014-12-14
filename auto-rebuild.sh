#!/bin/bash

# This script provides quick feedback on compilation errors by rebuilding the project whenever any
# source files change. Requires inotifywait.

set -e

SRC_DIR="$(dirname "${BASH_SOURCE[0]}")"
cd "$SRC_DIR"

git submodule init
git submodule update

(mkdir -p build && cd build && cmake ..)

while inotifywait -q -e attrib,close_write,create,delete,delete_self,move_self,moved_from,moved_to \
		$(find -regex './\(CMakeLists.txt\|\(src\|submodules\)/\(.*\.rs\|.*CMakeLists.txt\|.*\.cmake\)\)'); do

	reset
	echo -e "\n\nRebuilding...\n";
	(
		cd build &&
		make "$@" &&
		make run_tests
	) || true
done
