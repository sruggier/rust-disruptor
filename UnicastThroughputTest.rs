extern mod extra;
use extra::time::precise_time_ns;
use std::u64;
use std::task::{spawn_sched,SingleThreaded};

use disruptor::{SinglePublisher,SpinWaitStrategy};
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

/// Number of iterations to use on all benchmarks
static NUM_ITERATIONS: u64 = 1000 * 1000 * 100;
/// Expected value for the 99,999,999th triangle number
static EXPECTED_VALUE: u64 = 4999999950000000u64;

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
 * Single threaded version of the benchmark. Meaningless, because the compiler
 * evaluates the loop at compile time when optimizations are on.
 */
fn run_single_threaded_benchmark() {
    let before = precise_time_ns();
    let result = triangle_number(NUM_ITERATIONS-1);
    let ops = get_ops_per_second(before, NUM_ITERATIONS-1);
    assert!(result == EXPECTED_VALUE);
    println(fmt!("Single threaded: %? ops/sec", ops))
}

fn run_task_pipe_benchmark() {
    let iterations = NUM_ITERATIONS;

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

    // Send every number from 0 to NUM_ITERATIONS - 1, and then tell the task
    // to finish and return by sending uint::max_value.
    for num in range(0, iterations) {
        input_chan.send(num as u64);
    }
    input_chan.send(u64::max_value);
    // Wait for the task to finish
    let loop_end = precise_time_ns();
    let result = result_port.recv();
    let after = precise_time_ns();

    assert!(result == EXPECTED_VALUE);
    let ops = calculate_ops_per_second(before, after, iterations);
    let wait_latency = after - loop_end;
    println(fmt!("Pipes: %? ops/sec, result wait: %? ns", ops, wait_latency));
}

fn run_disruptor_benchmark() {
    let iterations = NUM_ITERATIONS;
    let mut publisher = SinglePublisher::<u64, SpinWaitStrategy>::new(8192, SpinWaitStrategy);
    let consumer = publisher.create_consumer_chain(1)[0];
    let (result_port, result_chan) = stream::<u64>();

    let before = precise_time_ns();

    // spawn_sched needed for now, because the consumer thread busy-waits
    // rather than voluntarily descheduling.
    do spawn_sched(SingleThreaded) {
        let mut sum = 0u64;

        loop {
            let mut i = u64::max_value;
            do consumer.consume |value: &u64| {
                i = *value;
            }
            debug!("%?", i);
            if i == u64::max_value {
                result_chan.send(sum);
                break;
            }
            sum += i;
        }
    }

    // Send value
    for num in range(0, iterations) {
        publisher.publish(num as u64)
    }
    publisher.publish(u64::max_value);

    let loop_end = precise_time_ns();
    let result = result_port.recv();
    let after = precise_time_ns();

    assert!(result == EXPECTED_VALUE);
    let ops = calculate_ops_per_second(before, after, iterations);
    let wait_latency = after - loop_end;
    println(fmt!("Pipes: %? ops/sec, result wait: %? ns", ops, wait_latency));
}

fn main() {
    run_single_threaded_benchmark();
    run_disruptor_benchmark();
    run_disruptor_benchmark();
    run_task_pipe_benchmark();
    run_task_pipe_benchmark();
}
