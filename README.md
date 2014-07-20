# A disruptor implementation for Rust

This is a library that allows for high-performance communication between Rust
tasks, fulfilling use cases similar to the built-in pipes API, but with better
performance. The core approach and many of the algorithms are based on ideas
from the [LMAX Disruptor](http://lmax-exchange.github.io/disruptor/) project.

# Features

This is a basic proof-of-concept, currently, so most functionality is missing.
However, current users can enjoy the following features:
 * Unicast pipelines consisting of a single publisher and one or more stages of
   consumer.
 * A general purpose blocking wait strategy that causes the receiver to
   eventually block if no items are sent
 * A spinning wait strategy that roughly triples performance compared to the
   blocking strategy.
 * A yielding wait strategy that has comparable performance to the spinning
   strategy when the CPU is idle, but doesn't waste as many CPU cycles when
   other software is executing.

# Building

The build system requires the following items in the path:
 * git (to clone the RustCMake submodule)
 * CMake 2.8
 * rustc

For those in a hurry, initiate the build with the following command:
	./build.sh

You can also build manually, with the following sequence of commands:
	git submodule init
	git submodule update

	mkdir -p build
	cd build
	cmake ..
	make

Optionally, you can pass `-j <jobs>` to run multiple build tasks in parallel
and save time.

The build will place built files under `build` in the following directories:
	lib     # the main library
	test    # executables that can be run with no arguments
	example # executables that can be run with --bench

The code is developed against Rust's master branch, and periodically ported to
newer versions. It's currently tested to work with rustc at the following
commit: 3a37ed4412f571a079e9d365c226af23dfe9d224

# License

To be compatible with Rust, this library is dual-licenced under the terms of the
MIT and Apache 2 licenses.

# Roadmap

This is a list of things that may be implemented in the near future:
 * Tests for unicast pipelines with more than one consumer
 * Porting benchmarks to Rust's built-in benchmark framework
 * Modularized source tree
 * Batch publishing APIs (it's supported in the core code, but not in the public
   API yet)
 * Consumers that process events from the same stage of the pipeline in parallel
   (in other words, each event is shared by all consumers)
 * Unsafe API to allow parallel consumers to mutate the data they're processing
   (it becomes the caller's responsibility to avoid data races)
 * If possible, a safe API to allow parallel consumers mutable access to
   separate parts of the data, using reflection to verify safety at runtime
   before running the pipeline
 * More unit tests in general
