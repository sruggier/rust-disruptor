#!/bin/bash

# TODO: port this to SCons, CMake, make, or the future rust packaging system

BUILD_DIR=./build

rm -rf "$BUILD_DIR"
mkdir -p "$BUILD_DIR"

# TODO: it looks like the dead code warnings no longer complain about items
# that are unused because they've been inlined. The remaining warnings should be
# fixed by adding more tests, and then the -A dead-code should be removed.
RUST_NONOPT_ARGS=(-A dead-code -g -L "$BUILD_DIR" -C prefer-dynamic)
RUST_OPT_ARGS=( "${RUST_NONOPT_ARGS[@]}" --opt-level 2)

RUST_LIB_ARGS=( "${RUST_OPT_ARGS[@]}" --out-dir="$BUILD_DIR" )
RUST_EXE_ARGS=( "${RUST_LIB_ARGS[@]}" )
RUST_TEST_ARGS=( "${RUST_NONOPT_ARGS[@]}" --test)
RUST_BENCH_ARGS=( "${RUST_LIB_ARGS[@]}" --test)

echo "Building disruptor library" &&
	rustc "${RUST_LIB_ARGS[@]}" src/disruptor/disruptor.rs &&
echo "Building disruptor tests" &&
	rustc "${RUST_TEST_ARGS[@]}" -o $BUILD_DIR/disruptor-tests src/disruptor/disruptor.rs &&
echo "Building UnicastThroughputTest.rs" &&
	rustc "${RUST_EXE_ARGS[@]}" src/tests/UnicastThroughputTest.rs &&
echo "Building disruptor benchmarks" &&
	rustc "${RUST_BENCH_ARGS[@]}" src/tests/benchmarks.rs
