// Copyright 2026 Simon Ruggier.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

#[macro_use]
extern crate log;

use std::fmt;
use std::hint::black_box;
use std::string;
use std::sync::mpsc::channel;
use std::thread::spawn;

use criterion::BenchmarkGroup;
use criterion::BenchmarkId;
use criterion::Criterion;
use criterion::criterion_group;
use criterion::criterion_main;
use criterion::measurement::WallTime;
use disruptor::{
    BlockingWaitStrategy, FinalConsumer, PipelineInit, ProcessingWaitStrategy, Publisher,
    SinglePublisher, SingleResizingPublisher, SpinWaitStrategy, YieldWaitStrategy,
};
use quanta::Instant;

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
* A slower version of triangle_number, where black_box has been used to deoptimize the loop.
*
* This ensures the CPU is executing every iteration of the loop, producing a more reasonable
* comparison for microbenchmarking purposes.
*/
fn triangle_number_slow(n: u64) -> u64 {
    let mut sum: u64 = 0;
    for num in 1..n + 1 {
        sum += num;
        black_box(sum);
    }
    sum
}

/**
 * Single threaded version of the benchmark. Returns the calculated value, for
 * use in other tests.
 */
fn bench_serial(g: &mut BenchmarkGroup<WallTime>) {
    g.bench_function("serial loop", |b| {
        b.iter_custom(|iterations| {
            let iteration_start = Instant::now();
            triangle_number_slow(iterations);
            iteration_start.elapsed()
        })
    });
}

fn bench_channel(g: &mut BenchmarkGroup<WallTime>) {
    g.bench_function("std::sync::mpsc::channel", |b| {
        b.iter_custom(|iterations| {
            let (result_sender, result_receiver) = channel::<u64>();
            let (input_sender, input_receiver) = channel::<u64>();

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

            let iteration_start = Instant::now();

            // Send every number from 1 to (iterations + 1), and then tell the task to finish and
            // return by sending usize::MAX.
            for num in 1..iterations + 1 {
                let result = input_sender.send(num);
                assert!(result.is_ok())
            }
            let result = input_sender.send(u64::MAX);
            assert!(result.is_ok());
            // Wait for the task to finish
            let sum = result_receiver.recv().unwrap();

            let elapsed_time = iteration_start.elapsed();

            let expected_value = triangle_number(iterations);
            assert_eq!(sum, expected_value);

            elapsed_time
        });
    });
}

fn bench_disruptor<P, FC, DisruptorFactory>(
    g: &mut BenchmarkGroup<WallTime>,
    create_disruptor: DisruptorFactory,
    desc: string::String,
) where
    P: Publisher<u64>,
    FC: FinalConsumer<u64> + 'static,
    DisruptorFactory: Fn() -> (P, FC),
{
    g.bench_function(BenchmarkId::new("disruptor", desc), |b| {
        b.iter_custom(|iterations| {
            let (publisher, consumer) = create_disruptor();
            let (result_sender, result_receiver) = channel::<u64>();

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

            let iteration_start = Instant::now();

            // Send every number from 1 to (iterations + 1), and then tell the task
            // to finish and return by sending usize::MAX.
            for num in 1..iterations + 1 {
                publisher.publish(num)
            }
            publisher.publish(u64::MAX);

            let result = result_receiver.recv().unwrap();
            let iteration_duration = iteration_start.elapsed();

            let expected_value = triangle_number(iterations);
            assert_eq!(result, expected_value);

            iteration_duration
        });
    });
}

fn run_nonresizing_disruptor_benchmark<W, WF>(
    g: &mut BenchmarkGroup<WallTime>,
    create_wait_strategy: WF,
) where
    W: ProcessingWaitStrategy + fmt::Debug + 'static,
    WF: Fn() -> W,
{
    let desc = format!("{:?}", create_wait_strategy());
    let create_disruptor = || {
        let mut publisher = SinglePublisher::<u64, 8192, W>::new(create_wait_strategy());
        let consumer = publisher.create_single_consumer_pipeline();
        (publisher, consumer)
    };
    bench_disruptor(g, create_disruptor, desc);
}

fn bench_disruptor_spin(g: &mut BenchmarkGroup<WallTime>) {
    // SpinWaitStrategy fully blocks the threads it's on, so the second task needs to be native to
    // avoid deadlock. Previously, deliberate action was needed to ensure this. Currently, though,
    // std::task::spawn spawns a native task by default, so no further action is necessary.
    run_nonresizing_disruptor_benchmark(g, || SpinWaitStrategy);
}

fn bench_disruptor_yield(g: &mut BenchmarkGroup<WallTime>) {
    run_nonresizing_disruptor_benchmark(g, YieldWaitStrategy::new);
}

fn bench_disruptor_block(g: &mut BenchmarkGroup<WallTime>) {
    run_nonresizing_disruptor_benchmark(g, BlockingWaitStrategy::new);
}

fn bench_disruptor_resizeable(g: &mut BenchmarkGroup<WallTime>) {
    let resize_timeout = 6;
    let mstp = disruptor::DEFAULT_MAX_SPIN_TRIES_PUBLISHER;
    let mstc = disruptor::DEFAULT_MAX_SPIN_TRIES_CONSUMER;
    let create_disruptor = || {
        let mut publisher = SingleResizingPublisher::<u64>::new_resize_after_timeout_with_params(
            8192,
            resize_timeout,
            mstp,
            mstc,
        );
        let consumer = publisher.create_single_consumer_pipeline();
        (publisher, consumer)
    };
    let desc = format!(
        "TimeoutResizeWaitStrategy{{t: {}, p: {}, c: {}}}",
        resize_timeout, mstp, mstc,
    );
    bench_disruptor(g, create_disruptor, desc);
}

fn bench_throughput(c: &mut Criterion) {
    let mut throughput_group = c.benchmark_group("throughput");
    throughput_group.throughput(criterion::Throughput::Elements(1));

    bench_serial(&mut throughput_group);
    bench_disruptor_resizeable(&mut throughput_group);
    bench_disruptor_block(&mut throughput_group);
    bench_disruptor_yield(&mut throughput_group);
    bench_disruptor_spin(&mut throughput_group);
    bench_channel(&mut throughput_group);
}
criterion_group!(benches, bench_throughput,);
criterion_main!(benches);
