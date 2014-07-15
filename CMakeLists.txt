# Copyright 2014 Simon Ruggier.
#
# Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
# http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
# <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
# option. This file may not be copied, modified, or distributed
# except according to those terms.
cmake_minimum_required(VERSION 2.8)
project(rust-disruptor NONE)

# Use SiegeLord's RustCMake repository for Rust support
list(APPEND CMAKE_MODULE_PATH "${CMAKE_SOURCE_DIR}/submodules/RustCMake/cmake")
find_package(rustc REQUIRED)
find_package(rustdoc)
include(Rust)

# Expose the built library to tests and examples
set(RUSTC_FLAGS -L "${CMAKE_BINARY_DIR}/lib")
set(RUSTDOC_FLAGS -L "${CMAKE_BINARY_DIR}/lib")

# More general rust flags
list(APPEND RUSTC_FLAGS
	# TODO: it looks like the dead code warnings no longer complain about items
	# that are unused because they've been inlined. The remaining warnings should be
	# fixed by adding more tests, and then the -A dead-code should be removed.
	-A dead-code
	# enable debug symbols
	-g
	# Use dynamic linking for any executables we build
	-C prefer-dynamic
	# We're gonna have to go right to... ludicrous speed!
	--opt-level 3
)

# build/install disruptor library
get_rust_deps(src/disruptor/disruptor.rs DISRUPTOR_LIB_DEPENDS)
rust_crate(
	src/disruptor/disruptor.rs
	TARGET_NAME DISRUPTOR_LIB
	DESTINATION lib
	DEPENDS ${DISRUPTOR_LIB_DEPENDS}
)

# Add library to default build target
add_custom_target(
	disruptor_library
	ALL
	DEPENDS ${DISRUPTOR_LIB_ARTIFACTS}
)

# build inline tests
rust_crate(
	src/disruptor/disruptor.rs
	TARGET_NAME DISRUPTOR_TESTS
	DESTINATION test
	DEPENDS "${DISRUPTOR_LIB_DEPENDS}"
	OTHER_RUSTC_FLAGS --test
)

# build unicast throughput benchmark
rust_crate_auto(
	src/tests/UnicastThroughputTest.rs
	TARGET_NAME UNICAST_THROUGHPUT_TEST
	DEPENDS ${DISRUPTOR_LIB_ARTIFACTS}
	DESTINATION test
)

rust_crate_auto(
	src/tests/benchmarks.rs
	TARGET_NAME BENCHMARKS
	DESTINATION bench
	DEPENDS ${DISRUPTOR_LIB_ARTIFACTS}
	OTHER_RUSTC_FLAGS --test
)

# build tests, benchmarks, and examples by default
add_custom_target(
	disruptor_tests
	ALL
	DEPENDS 
		${DISRUPTOR_TESTS_ARTIFACTS}
		${BENCHMARKS_ARTIFACTS}
		${UNICAST_THROUGHPUT_TEST_ARTIFACTS}
)

# Include the built library in the search path, so that the tests can run
set(LIBRARY_SEARCH_PATH "${CMAKE_BINARY_DIR}/lib")
set(LIBRARY_SEARCH_PATH "${LIBRARY_SEARCH_PATH}:$ENV{LD_LIBRARY_PATH}")

# Add a test target to build and run the remaining items. It's not supported to run these in
# parallel.
add_custom_target(
	run_tests
	COMMENT "Running tests and benchmarks"
	COMMAND test/disruptor
	COMMAND env LD_LIBRARY_PATH=${LIBRARY_SEARCH_PATH} bench/benchmarks --bench
	COMMAND env LD_LIBRARY_PATH=${LIBRARY_SEARCH_PATH} test/UnicastThroughputTest
	VERBATIM
	DEPENDS disruptor_tests
)
