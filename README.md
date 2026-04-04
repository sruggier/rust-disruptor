# A disruptor implementation for Rust

This is a library crate that facilitates high-performance communication between
Rust threads, inspired by the [LMAX
Disruptor](http://lmax-exchange.github.io/disruptor/) project.

## Features

This is a basic proof-of-concept, currently, so most functionality is missing.
However, current users can enjoy the following features:

* Unicast pipelines consisting of a single publisher and one or more stages of
consumer.
* Various wait strategies:
  - SpinWaitStrategy: spins indefinitely. Useful, along with thread pinning,
  when minimizing latency is more important than efficient hardware
  utilization.
  - YieldWaitStrategy: spins briefly, then yields in between each check. This
  results in somewhat better hardware utilization, at the cost of higher
  latency.
  - BlockingWaitStrategy: like YieldWaitStrategy, except that consumers
  eventually sleep on a wait condition if a timeout is reached. This is much
  more efficient if there are long periods of time where no items are
  published, but it comes with higher latency, and imposes a performance cost
  on the publishing thread, which has to check for sleeping consumers on every
  publish, and, if needed, wake them up. This crate includes an optimized
  implementation, relative to the LMAX disruptor, where the publishing thread
  uses a read-modify-write operation to skip taking the lock unless a consumer
  is waiting.
* Optional support for dynamic reallocation/resizing of the ring buffer during
publishing, if a timeout is reached. This has the potential to do more harm
than good, by almost entirely eliminating back-pressure on the publisher. It is
left as an exercise for the reader to try to find a use case where this makes
sense.

## Building

The build system requires the following items in the path:

* cargo
* rustc
* Optional: inotifywait (for auto-rebuild.sh)

You can build, run tests, and run benchmarks using `cargo build`, `cargo test`,
and `cargo bench`, respectively.

## Hacking

The auto-rebuild.sh script rebuilds whenever any source files change. Leave it
open in a terminal as you hack on the code for convenient feedback about compile
errors, test results, and performance.

## License

For compatibility with the Rust ecosystem, this library is dual-licensed under
the terms of the MIT and Apache 2 licenses.
