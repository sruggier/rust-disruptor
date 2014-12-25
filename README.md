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

The The build system requires the following items in the path:
 * cargo
 * rustc
 * Optional: inotifywait (for auto-rebuild.sh)

The code is developed against Rust's master branch, and periodically ported to
newer versions. It's currently tested to work with rustc at commit
`8a33de89c4a7acf04a2f3fa5d6ba4aa3fe3f8dc0`

You can build, run tests, and run benchmarks using `cargo build`, `cargo test`,
and `cargo bench`, respectively. Additionally, `cargo test` builds
examples/UnicastThroughputTest, which is a simple throughput benchmark that
compares rust's channel API to the disruptor implementation using various wait
strategies.

# Hacking

The auto-rebuild.sh script rebuilds whenever any source files change. Leave it
open in a terminal as you hack on the code for convenient feedback about compile
errors, test results, and performance.

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
