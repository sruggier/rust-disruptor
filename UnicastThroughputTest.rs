// Copyright 2013 Simon Ruggier.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.
extern mod extra;
use extra::getopts;
use extra::time::precise_time_ns;
use std::fmt;
use std::u64;
use std::task::{spawn_sched,SchedMode,SingleThreaded,DefaultScheduler};

use disruptor::{Publisher,ProcessingWaitStrategy,SpinWaitStrategy,YieldWaitStrategy,BlockingWaitStrategy};
mod disruptor;

/**
 * Given a start time, finish time, and number of iterations, calculates and
 * returns the number of operations per second.
 */
fn calculate_ops_per_second(before: u64, after: u64, iterations: u64) -> u64 {
    1000*1000*1000*iterations/(after-before)
}

/**
 * Given a start time and a number of iterations, retrieves the current time
 * and returns the number of operations per second.
 */
fn get_ops_per_second(before: u64, iterations: u64) -> u64 {
    let after = precise_time_ns();
    calculate_ops_per_second(before, after, iterations)
}

/// Default number of iterations to use on all benchmarks
static NUM_ITERATIONS: u64 = 1000 * 1000 * 100 - 1;

/**
 * Calculates the nth triangle number by summing the numbers from 1 to n in a
 * loop. Note that the compiler appears to evaluate this at compile time at opt
 * levels 2 and above.
 */
fn triangle_number(n: u64) -> u64 {
    let mut sum : u64 = 0;
    for num in range(1, n+1) {
        sum += num as u64;
    }
    sum
}

/**
 * Single threaded version of the benchmark. Returns the calculated value, for
 * use in other tests.
 */
fn run_single_threaded_benchmark(iterations: u64) -> u64 {
    let before = precise_time_ns();
    let result = triangle_number(iterations);
    let ops = get_ops_per_second(before, iterations);
    println!("Single threaded: {:?} ops/sec (result was {})", ops, result);

    result
}

fn run_task_pipe_benchmark(iterations: u64) {

    let (result_port, result_chan) = stream::<u64>();
    let (input_port, input_chan) = stream::<u64>();

    let before = precise_time_ns();

    // Listen on input_port, summing all the received numbers, then return the
    // sum through result_chan.
    do spawn {
        let mut sum = 0u64;
        let mut i = input_port.recv();
        while i != u64::max_value {
            sum += i;
            i = input_port.recv();
        }
        result_chan.send(sum);
    }

    // Send every number from 1 to (iterations + 1), and then tell the task
    // to finish and return by sending uint::max_value.
    for num in range(1, iterations + 1) {
        input_chan.send(num as u64);
    }
    input_chan.send(u64::max_value);
    // Wait for the task to finish
    let loop_end = precise_time_ns();
    let result = result_port.recv();
    let after = precise_time_ns();

    let expected_value = triangle_number(iterations);
    assert_eq!(result, expected_value);
    let ops = calculate_ops_per_second(before, after, iterations);
    let wait_latency = after - loop_end;
    println!("Pipes: {:?} ops/sec, result wait: {:?} ns", ops, wait_latency);
}

fn run_disruptor_benchmark<W: ProcessingWaitStrategy + fmt::Default>(iterations: u64, w: W, mode: SchedMode) {
    // generate a formatted string representation of w before it's moved into publisher
    let wait_str = format!("{}", w);
    let mut publisher = Publisher::<u64, W>::new(8192, w);
    let consumer = publisher.create_single_consumer_pipeline();
    let (result_port, result_chan) = stream::<u64>();

    let before = precise_time_ns();

    // spawn_sched needed for now, because the consumer thread busy-waits
    // rather than voluntarily descheduling.
    do spawn_sched(mode) {
        let mut sum = 0u64;

        loop {
            let i = consumer.take();
            debug!("{:?}", i);
            // In-band magic number value tells us when to break out of the loop
            if i == u64::max_value {
                result_chan.send(sum);
                break;
            }
            sum += i;
        }
    }

    // Send every number from 1 to (iterations + 1), and then tell the task
    // to finish and return by sending uint::max_value.
    for num in range(1, iterations + 1) {
        publisher.publish(num as u64)
    }
    publisher.publish(u64::max_value);

    let loop_end = precise_time_ns();
    let result = result_port.recv();
    let after = precise_time_ns();

    let expected_value = triangle_number(iterations);
    assert_eq!(result, expected_value);
    let ops = calculate_ops_per_second(before, after, iterations);
    let wait_latency = after - loop_end;
    println!("Disruptor ({}): {} ops/sec, result wait: {} ns", wait_str, ops, wait_latency);
}

fn run_disruptor_benchmark_spin(iterations: u64) {
    run_disruptor_benchmark(iterations, SpinWaitStrategy, SingleThreaded);
}

fn run_disruptor_benchmark_yield(iterations: u64) {
    run_disruptor_benchmark(iterations, YieldWaitStrategy::new(), DefaultScheduler);
}

fn run_disruptor_benchmark_block(iterations: u64) {
    run_disruptor_benchmark(iterations, BlockingWaitStrategy::new(), DefaultScheduler);
}

fn usage(argv0: &str, opts: ~[getopts::groups::OptGroup]) -> ! {
    let brief = format!("Usage: {} [OPTIONS]", argv0);
    println!("{}", getopts::groups::usage(brief, opts));
    // Exit immediately
    fail!();
}

/**
 * Retrieve a parsed representation of the command-line arguments, or die
 * trying. If the user has requested a help string or given an invalid
 * argument, this will print out help information and exit.
 */
fn parse_args() -> getopts::Matches {
    use extra::getopts::groups::{optflag,optopt};

    let opts = ~[
        optflag("h", "help", "show this message and exit"),
        optopt("n", "iterations", "how many iterations to perform in each benchmark", "N"),
    ];

    let args = std::os::args();
    let arg_flags = args.tail();
    let argv0 = &args[0];

    let matches = match getopts::groups::getopts(arg_flags, opts) {
        Ok(m) => m,
        Err(fail) => {
            println!("{}\nUse '{} --help' to see a list of valid options.", fail.to_err_msg(), *argv0);
            fail!();
        }
    };
    if (matches.opt_present("h")) {
        usage(*argv0, opts);
    }

    matches
}

fn main() {
    let matches = parse_args();

    let iterations = match matches.opt_str("n") {
        Some(n_str) => {
            match std::u64::parse_bytes(n_str.as_bytes(), 10u) {
                Some(n) => n,
                None => fail!("Expected a positive number of iterations, received {}", n_str)
            }
        }
        None => NUM_ITERATIONS
    };

    run_single_threaded_benchmark(iterations);
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
    run_task_pipe_benchmark(iterations/100);
    run_task_pipe_benchmark(iterations/100);
}
