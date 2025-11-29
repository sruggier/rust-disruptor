// Copyright 2014 Simon Ruggier.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

#[macro_use]
extern crate log;

use std::convert::TryFrom;
use std::fmt;
use std::string;
use std::sync::mpsc::channel;
use std::thread::spawn;
use std::time::Duration;
use std::time::Instant;
use std::u64;

use crate::benchmark_utils::parse_args;
use disruptor::{
    BlockingWaitStrategy, FinalConsumer, PipelineInit, ProcessingWaitStrategy, Publisher,
    SinglePublisher, SingleResizingPublisher, SpinWaitStrategy, YieldWaitStrategy,
};
#[path = "../src/benchmark_utils.rs"]
mod benchmark_utils;

/**
 * Given a start time, finish time, and number of iterations, calculates and
 * returns the number of operations per second.
 */
fn calculate_ops_per_second(duration: Duration, iterations: u64) -> u64 {
    // Support for benchmarks long enough to overflow u64 is planned for
    // completion by the end of Q3 in 2554.
    let duration_u64 =
        u64::try_from(duration.as_nanos()).expect("duration was too long to store in u64");
    1000 * 1000 * 1000 * iterations / duration_u64
}

/**
 * Given a start time and a number of iterations, retrieves the current time
 * and returns the number of operations per second.
 */
fn get_ops_per_second(start: Instant, iterations: u64) -> u64 {
    let duration = start.elapsed();
    calculate_ops_per_second(duration, iterations)
}

/// Default number of iterations to use on all benchmarks
static NUM_ITERATIONS: u64 = 1000 * 1000 * 100 - 1;

/**
 * Calculates the nth triangle number by summing the numbers from 1 to n in a
 * loop. Note that the compiler appears to evaluate this at compile time at opt
 * levels 2 and above.
 */
fn triangle_number(n: u64) -> u64 {
    let mut sum: u64 = 0;
    for num in 1..n + 1 {
        sum += num;
    }
    sum
}

/**
 * Single threaded version of the benchmark. Returns the calculated value, for
 * use in other tests.
 */
fn run_single_threaded_benchmark(iterations: u64) -> u64 {
    let before = Instant::now();
    let result = triangle_number(iterations);
    let ops = get_ops_per_second(before, iterations);
    println!("Single threaded: {} ops/sec (result was {})", ops, result);

    result
}

fn run_task_pipe_benchmark(iterations: u64) {
    let (result_sender, result_receiver) = channel::<u64>();
    let (input_sender, input_receiver) = channel::<u64>();

    let before = Instant::now();

    // Listen on input_receiver, summing all the received numbers, then return the
    // sum through result_sender.
    spawn(move || {
        let mut sum = 0u64;
        let mut i = input_receiver.recv().unwrap();
        while i != u64::MAX {
            sum += i;
            i = input_receiver.recv().unwrap();
        }
        let result = result_sender.send(sum);
        assert!(result.is_ok());
    });

    // Send every number from 1 to (iterations + 1), and then tell the task
    // to finish and return by sending usize::MAX.
    for num in 1..iterations + 1 {
        let result = input_sender.send(num);
        assert!(result.is_ok())
    }
    let result = input_sender.send(u64::MAX);
    assert!(result.is_ok());
    // Wait for the task to finish
    let loop_end = Instant::now();
    let result = result_receiver.recv().unwrap();
    let after = Instant::now();

    let expected_value = triangle_number(iterations);
    assert_eq!(result, expected_value);
    let ops = calculate_ops_per_second(after - before, iterations);
    let wait_latency = (after - loop_end).as_nanos();
    println!("Pipes: {} ops/sec, result wait: {} ns", ops, wait_latency);
}

fn run_disruptor_benchmark<P: Publisher<u64>, FC: FinalConsumer<u64> + 'static>(
    iterations: u64,
    publisher: P,
    consumer: FC,
    desc: string::String,
) {
    let (result_sender, result_receiver) = channel::<u64>();

    let before = Instant::now();

    spawn(move || {
        let mut sum = 0u64;

        let mut expected_value = 1u64;
        loop {
            let i = consumer.take();
            debug!("{}", i);
            // In-band magic number value tells us when to break out of the loop
            if i == u64::MAX {
                let result = result_sender.send(sum);
                assert!(result.is_ok());
                break;
            }
            assert_eq!(i, expected_value);
            expected_value += 1;
            sum += i;
        }
    });

    // Send every number from 1 to (iterations + 1), and then tell the task
    // to finish and return by sending usize::MAX.
    for num in 1..iterations + 1 {
        publisher.publish(num)
    }
    publisher.publish(u64::MAX);

    let loop_end = Instant::now();
    let result = result_receiver.recv().unwrap();
    let after = Instant::now();

    let expected_value = triangle_number(iterations);
    assert_eq!(result, expected_value);
    let ops = calculate_ops_per_second(after - before, iterations);
    let wait_latency = after - loop_end;
    println!(
        "Disruptor ({}): {} ops/sec, result wait: {} ns",
        desc,
        ops,
        wait_latency.as_nanos()
    );
}

fn run_nonresizing_disruptor_benchmark<W: ProcessingWaitStrategy + fmt::Debug + 'static>(
    iterations: u64,
    w: W,
) {
    let desc = format!("{:?}", w);
    let mut publisher = SinglePublisher::<u64, W>::new(8192, w);
    let consumer = publisher.create_single_consumer_pipeline();
    run_disruptor_benchmark(iterations, publisher, consumer, desc);
}

fn run_disruptor_benchmark_spin(iterations: u64) {
    // SpinWaitStrategy fully blocks the threads it's on, so the second task needs to be native to
    // avoid deadlock. Previously, deliberate action was needed to ensure this. Currently, though,
    // std::task::spawn spawns a native task by default, so no further action is necessary.
    run_nonresizing_disruptor_benchmark(iterations, SpinWaitStrategy);
}

fn run_disruptor_benchmark_yield(iterations: u64) {
    run_nonresizing_disruptor_benchmark(iterations, YieldWaitStrategy::new());
}

fn run_disruptor_benchmark_block(iterations: u64) {
    run_nonresizing_disruptor_benchmark(iterations, BlockingWaitStrategy::new());
}

fn run_disruptor_benchmark_resizeable(iterations: u64) {
    let resize_timeout = 6;
    let mstp = disruptor::DEFAULT_MAX_SPIN_TRIES_PUBLISHER;
    let mstc = disruptor::DEFAULT_MAX_SPIN_TRIES_CONSUMER;
    let mut publisher = SingleResizingPublisher::<u64>::new_resize_after_timeout_with_params(
        8192,
        resize_timeout,
        mstp,
        mstc,
    );
    let consumer = publisher.create_single_consumer_pipeline();
    let desc = format!(
        "disruptor::TimeoutResizeWaitStrategy{{t: {}, p: {}, c: {}}}",
        resize_timeout, mstp, mstc
    );
    run_disruptor_benchmark(iterations, publisher, consumer, desc);
}

fn main() {
    // Default to NUM_ITERATIONS
    let common_opts = parse_args(NUM_ITERATIONS);
    let iterations = common_opts.n_iterations;

    run_single_threaded_benchmark(iterations);
    run_disruptor_benchmark_resizeable(iterations);
    run_disruptor_benchmark_block(iterations);
    run_disruptor_benchmark_block(iterations);
    run_disruptor_benchmark_block(iterations);
    run_disruptor_benchmark_yield(iterations);
    run_disruptor_benchmark_yield(iterations);
    run_disruptor_benchmark_yield(iterations);
    run_disruptor_benchmark_spin(iterations);
    run_disruptor_benchmark_spin(iterations);
    run_disruptor_benchmark_spin(iterations);
    // The pipes are slower, so we avoid long execution times by running fewer iterations
    run_task_pipe_benchmark(iterations / 100);
    run_task_pipe_benchmark(iterations / 100);
}
