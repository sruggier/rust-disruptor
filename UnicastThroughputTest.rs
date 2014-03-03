// Copyright 2013 Simon Ruggier.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.
extern crate extra;
extern crate getopts;
extern crate time;
use time::precise_time_ns;
use std::fmt;
use std::u64;
use std::task::{spawn};

use disruptor::{Publisher,FinalConsumer,ProcessingWaitStrategy,SpinWaitStrategy,YieldWaitStrategy,BlockingWaitStrategy, SequenceBarrier};
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

    let (result_port, result_chan) = Chan::<u64>::new();
    let (input_port, input_chan) = Chan::<u64>::new();

    let before = precise_time_ns();

    // Listen on input_port, summing all the received numbers, then return the
    // sum through result_chan.
    spawn(proc() {
        let mut sum = 0u64;
        let mut i = input_port.recv();
        while i != u64::MAX {
            sum += i;
            i = input_port.recv();
        }
        result_chan.send(sum);
    });

    // Send every number from 1 to (iterations + 1), and then tell the task
    // to finish and return by sending uint::MAX.
    for num in range(1, iterations + 1) {
        input_chan.send(num as u64);
    }
    input_chan.send(u64::MAX);
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

fn run_disruptor_benchmark<SB: SequenceBarrier<u64>, CSB: SequenceBarrier<u64>>(
    iterations: u64,
    publisher: Publisher<SB>,
    consumer: FinalConsumer<CSB>,
    desc: ~str
) {
    let (result_port, result_chan) = Chan::<u64>::new();

    let before = precise_time_ns();

    // spawn_sched needed for now, because the consumer thread busy-waits
    // rather than voluntarily descheduling.
    spawn(proc() {
        let mut sum = 0u64;

        let mut expected_value = 1u64;
        loop {
            let i = consumer.take();
            debug!("{:?}", i);
            // In-band magic number value tells us when to break out of the loop
            if i == u64::MAX {
                result_chan.send(sum);
                break;
            }
            assert_eq!(i, expected_value);
            expected_value += 1;
            sum += i;
        }
    });

    // Send every number from 1 to (iterations + 1), and then tell the task
    // to finish and return by sending uint::MAX.
    for num in range(1, iterations + 1) {
        publisher.publish(num as u64)
    }
    publisher.publish(u64::MAX);

    let loop_end = precise_time_ns();
    let result = result_port.recv();
    let after = precise_time_ns();

    let expected_value = triangle_number(iterations);
    assert_eq!(result, expected_value);
    let ops = calculate_ops_per_second(before, after, iterations);
    let wait_latency = after - loop_end;
    println!("Disruptor ({}): {} ops/sec, result wait: {} ns", desc, ops, wait_latency);
}

fn run_nonresizing_disruptor_benchmark<W: ProcessingWaitStrategy + fmt::Show>(
    iterations: u64,
    w: W
) {
    let desc = format!("{}", w);
    let mut publisher = Publisher::<u64, W>::new(8192, w);
    let consumer = publisher.create_single_consumer_pipeline();
    run_disruptor_benchmark(iterations, publisher, consumer, desc);
}

fn run_disruptor_benchmark_spin(iterations: u64) {
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
    let mstp = disruptor::default_max_spin_tries_publisher;
    let mstc = disruptor::default_max_spin_tries_consumer;
    let mut publisher = Publisher::<u64>::new_resize_after_timeout_with_params(
        8192,
        resize_timeout,
        mstp,
        mstc
    );
    let consumer = publisher.create_single_consumer_pipeline();
    let desc = format!("disruptor::TimeoutResizeWaitStrategy\\{t: {}, p: {}, c: {}\\}",
        resize_timeout,
        mstp,
        mstc
    );
    run_disruptor_benchmark(iterations, publisher, consumer, desc);;
}

fn usage(argv0: &str, opts: ~[getopts::OptGroup]) -> ! {
    let brief = format!("Usage: {} [OPTIONS]", argv0);
    println!("{}", getopts::usage(brief, opts));
    // Exit immediately
    fail!();
}

/**
 * Retrieve a parsed representation of the command-line arguments, or die
 * trying. If the user has requested a help string or given an invalid
 * argument, this will print out help information and exit.
 */
fn parse_args() -> getopts::Matches {
    use getopts::{optflag,optopt};

    let opts = ~[
        optflag("h", "help", "show this message and exit"),
        optopt("n", "iterations", "how many iterations to perform in each benchmark", "N"),
    ];

    let args = std::os::args();
    let arg_flags = args.tail();
    let argv0 = &args[0];

    let matches = match getopts::getopts(arg_flags, opts) {
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
    run_disruptor_benchmark_resizeable(iterations);
    run_disruptor_benchmark_block(iterations);
    run_disruptor_benchmark_block(iterations);
    run_disruptor_benchmark_block(iterations);
    run_disruptor_benchmark_yield(iterations);
    run_disruptor_benchmark_yield(iterations);
    run_disruptor_benchmark_yield(iterations);
    // TODO: 1:1 scheduling of tasks is needed for SpinWaitStrategy to work,
    // but I ripped out the code to do that while porting to latest rustc as of
    // 2014-03-02. Reimplement using libgreen.
    // run_disruptor_benchmark_spin(iterations);
    // run_disruptor_benchmark_spin(iterations);
    // run_disruptor_benchmark_spin(iterations);
    // The pipes are slower, so we avoid long execution times by running fewer iterations
    run_task_pipe_benchmark(iterations/100);
    run_task_pipe_benchmark(iterations/100);
}
