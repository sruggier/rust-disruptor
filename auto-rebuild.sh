#!/bin/bash

# This script provides quick feedback on compilation errors by rebuilding the project whenever any
# source files change. Requires inotifywait.

set -e

SRC_DIR="$(dirname "${BASH_SOURCE[0]}")"
cd "$SRC_DIR"

while inotifywait -q -e attrib,close_write,create,delete,delete_self,move_self,moved_from,moved_to \
		$(find -regex './\(Cargo.toml\|\(src\|examples\|tests\|benches\)/\(.*\.rs\)\)'); do

	reset
	echo -e "\n\nRebuilding...\n";
	(
		cargo build &&
		cargo test &&
		cargo bench &&
		./target/examples/unicast_throughput_benchmark
	) || true
done
