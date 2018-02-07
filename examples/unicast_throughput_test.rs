// Copyright 2014 Simon Ruggier.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

extern crate disruptor;
#[macro_use]
extern crate log;
extern crate time;
extern crate getopts;

use self::getopts::Options;
use std::str::FromStr;

use time::precise_time_ns;
use std::{fmt, string, thread};
use std::sync::mpsc::channel;
use fmt::Debug;

use disruptor::{SinglePublisher, SingleResizingPublisher, ProcessingWaitStrategy,SpinWaitStrategy,
    YieldWaitStrategy,BlockingWaitStrategy, PipelineInit, Publisher, FinalConsumer, Consumer};

/// Contains values obtained from common argument processing.
pub struct CommonTestOpts {
    pub n_iterations: u64,
}

fn usage(argv0: &str, opts: Options) -> ! {
    let brief = format!("Usage: {} [OPTIONS]", argv0);
    println!("{}", opts.usage(&brief));
    // Exit immediately
    panic!();
}

/**
 * Retrieve a parsed representation of the command-line arguments, or die trying. If the user has
 * requested a help string or given an invalid argument, this will print out help information and
 * exit.
 *
 * The API of this function will clearly have to change if some callers want to include their own
 * arguments, but for now, this will do.
 */
pub fn parse_args(default_n_iterations: u64) -> CommonTestOpts {
    let mut opts = Options::new();
    opts.optflag("h", "help", "show this message and exit");
    opts.optopt("n", "iterations",
                &format!("how many iterations to perform in each benchmark (default {})",
                         default_n_iterations), "N");

    let args = ::std::env::args().collect::<Vec<String>>();
    let arg_flags = &args[1..];
    let argv0 = &args[0];

    let matches = match opts.parse(arg_flags) {
        Ok(m) => m,
        Err(fail) => {
            println!("{}\nUse '{} --help' to see a list of valid options.", fail, argv0);
            panic!();
        }
    };
    if matches.opt_present("h") {
        usage(&argv0, opts);
    }

    // Validate as integer if -n specified
    let iterations = matches.opt_str("n")
        .map(|n_str|
            u64::from_str(&n_str)
                .expect("Expected a positive number of iterations")
        )
        .unwrap_or(default_n_iterations);

    CommonTestOpts {
        n_iterations: iterations,
    }
}


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
    for num in 1..(n+1) {
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
    println!("Single threaded: {} ops/sec (result was {})", ops, result);

    result
}

fn run_task_pipe_benchmark(iterations: u64) {

    let (result_sender, result_receiver) = channel::<u64>();
    let (input_sender, input_receiver) = channel::<u64>();

    let before = precise_time_ns();

    // Listen on input_receiver, summing all the received numbers, then return the
    // sum through result_sender.
    thread::spawn(move || {
        let mut sum = 0u64;
        let mut i = input_receiver.recv().unwrap();
        while i != u64::max_value() {
            sum += i;
            i = input_receiver.recv().unwrap();
        }
        result_sender.send(sum).unwrap();
    });

    // Send every number from 1 to (iterations + 1), and then tell the task
    // to finish and return by sending uint::MAX.
    for num in 1..(iterations + 1) {
        input_sender.send(num as u64).unwrap();
    }
    input_sender.send(u64::max_value()).unwrap();
    // Wait for the task to finish
    let loop_end = precise_time_ns();
    let result = result_receiver.recv().unwrap();
    let after = precise_time_ns();

    let expected_value = triangle_number(iterations);
    assert_eq!(result, expected_value);
    let ops = calculate_ops_per_second(before, after, iterations);
    let wait_latency = after - loop_end;
    println!("Pipes: {} ops/sec, result wait: {} ns", ops, wait_latency);
}

fn run_disruptor_benchmark<C: Consumer<u64>, FC: FinalConsumer<u64> + 'static, P: Publisher<u64> + PipelineInit<u64, C, FC>>(
    iterations: u64,
    mut publisher: P,
    desc: string::String
) {
    let (result_sender, result_receiver) = channel::<u64>();

    let before = precise_time_ns();

    let consumer = publisher.create_single_consumer_pipeline();
    thread::spawn(move || {
        let mut sum = 0u64;

        let mut expected_value = 1u64;
        loop {
            let i = consumer.take();
            debug!("{}", i);
            // In-band magic number value tells us when to break out of the loop
            if i == u64::max_value() {
                result_sender.send(sum).unwrap();
                break;
            }
            assert_eq!(i, expected_value);
            expected_value += 1;
            sum += i;
        }
    });

    // Send every number from 1 to (iterations + 1), and then tell the task
    // to finish and return by sending uint::MAX.
    for num in 1..(iterations + 1) {
        publisher.publish(num as u64)
    }
    publisher.publish(u64::max_value());

    let loop_end = precise_time_ns();
    let result = result_receiver.recv().unwrap();
    let after = precise_time_ns();

    let expected_value = triangle_number(iterations);
    assert_eq!(result, expected_value);
    let ops = calculate_ops_per_second(before, after, iterations);
    let wait_latency = after - loop_end;
    println!("Disruptor ({}): {} ops/sec, result wait: {} ns", desc, ops, wait_latency);
}

fn run_nonresizing_disruptor_benchmark<W: ProcessingWaitStrategy + Debug + 'static>(
    iterations: u64,
    w: W
) {
    let desc = format!("{:?}", w);
    let publisher = SinglePublisher::<u64, W>::new(8192, w);
    run_disruptor_benchmark(iterations, publisher, desc);
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
    let publisher = SingleResizingPublisher::<u64>::new_resize_after_timeout_with_params(
        8192,
        resize_timeout,
        mstp,
        mstc
    );
    let desc = format!("disruptor::TimeoutResizeWaitStrategy{{t: {}, p: {}, c: {}}}",
        resize_timeout,
        mstp,
        mstc
    );
    run_disruptor_benchmark(iterations, publisher, desc);;
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
    run_task_pipe_benchmark(iterations/100);
    run_task_pipe_benchmark(iterations/100);
}
