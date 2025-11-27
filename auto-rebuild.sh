#!/bin/bash

# This script provides quick feedback on compilation errors by rebuilding the project whenever any
# source files change. Requires inotifywait.

set -e

function usage() {
	CMDNAME=$(basename $0)
	echo "usage:
$CMDNAME [-n]

options:
    -u            Enable features that depend on unstable, namely benchmarking
"
	exit 2
}

UNSTABLE=0

while getopts "uh" ARG; do
	case $ARG in
	u)
		UNSTABLE=1
		;;
	h | ?)
		usage
		;;
	esac
done

SRC_DIR="$(dirname "${BASH_SOURCE[0]}")"
cd "$SRC_DIR"

while inotifywait -q -e attrib,close_write,create,delete,delete_self,move_self,moved_from,moved_to \
	$(find -regex './\(Cargo.toml\|\(src\|examples\|tests\|benches\)/\(.*\.rs\)\)'); do

	reset
	echo -e "\n\nRebuilding...\n"
	(
		cargo build &&
			cargo test &&
			if ((UNSTABLE)); then
				cargo bench
			fi &&
			./target/debug/examples/unicast_throughput_benchmark -n 1000000
	) || true
done
