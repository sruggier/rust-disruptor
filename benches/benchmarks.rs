use criterion::{Bencher, Criterion, criterion_group, criterion_main};
use disruptor::{
    BlockingWaitStrategy, Consumer, FinalConsumer, PipelineInit, ProcessingWaitStrategy, Publisher,
    SinglePublisher, SpinWaitStrategy, YieldWaitStrategy,
};
use std::{
    hint::black_box,
    thread::{spawn, yield_now},
    time::{Duration, Instant},
};

fn iter_latency<O, R>(b: &mut Bencher, mut routine: R)
where
    R: FnMut() -> O,
{
    const PAUSE_DURATION: Duration = Duration::from_micros(1);

    b.iter_custom(move |iters| {
        let mut total_time = Duration::ZERO;
        for _i in 0..iters {
            let latency_start = Instant::now();
            black_box(routine());
            total_time += latency_start.elapsed();

            // Pause for 1 µs after each measurement, to show the worst-case latency from each
            // wait strategy.
            let pause_start = Instant::now();
            while pause_start.elapsed() < PAUSE_DURATION {
                yield_now();
            }
        }
        total_time
    });
}

/**
 * Run a two-disruptor ping-pong latency benchmark with the given wait strategy and spawn function.
 *
 * # Arguments
 *
 * * c - the benchmark manager
 * * w - The wait strategy to use
 */
fn measure_ping_pong_latency_two_ringbuffers_generic<W: ProcessingWaitStrategy + 'static>(
    flavour: &str,
    c: &mut Criterion,
    w: W,
) {
    let mut ping_publisher = SinglePublisher::<u64, W>::new(8192, w.clone());
    let ping_consumer = ping_publisher.create_single_consumer_pipeline();
    let mut pong_publisher = SinglePublisher::<u64, W>::new(8192, w.clone());
    let pong_consumer = pong_publisher.create_single_consumer_pipeline();

    spawn(move || {
        loop {
            // Echo every received value
            let i = ping_consumer.take();
            // In-band magic value indicates that we should exit
            if u64::MAX == i {
                break;
            } else {
                pong_publisher.publish(i);
            }
        }
    });

    let mut i = 0;

    let bench_id = format!("two-ringbuffer ping pong latency ({})", flavour);
    c.bench_function(bench_id.as_str(), |b| {
        iter_latency(b, || {
            ping_publisher.publish(i);
            let i_echo = pong_consumer.take();
            assert_eq!(i, i_echo);
            i += 1;
        })
    });
    ping_publisher.publish(u64::MAX);
}

fn measure_ping_pong_latency_two_ringbuffers_spin(c: &mut Criterion) {
    let w = SpinWaitStrategy;
    measure_ping_pong_latency_two_ringbuffers_generic("spin", c, w);
}

fn measure_ping_pong_latency_two_ringbuffers_yield(c: &mut Criterion) {
    let w = YieldWaitStrategy::new();
    measure_ping_pong_latency_two_ringbuffers_generic("yield", c, w);
}

fn measure_ping_pong_latency_two_ringbuffers_block(c: &mut Criterion) {
    let w = BlockingWaitStrategy::new();
    measure_ping_pong_latency_two_ringbuffers_generic("block", c, w);
}

/**
 * Run a one-disruptor ping-pong latency benchmark with the given wait strategy and spawn function.
 * In this version, a single disruptor is used to synchronize the two tasks, which avoids some
 * redundancy.
 *
 * # Arguments
 *
 * * b - the Bencher
 * * w - The wait strategy to use
 */
fn measure_ping_pong_latency_one_ringbuffer_generic<W: ProcessingWaitStrategy + 'static>(
    flavour: &str,
    c: &mut Criterion,
    w: W,
) {
    let mut ping_publisher = SinglePublisher::<u64, W>::new(8192, w.clone());

    // The second task listens for items from ping_consumer, and the publisher waits for the ping to
    // be processed by listening on pong_consumer before publishing the next item.
    let (mut ping_consumer_vec, pong_consumer) = ping_publisher.create_consumer_pipeline(2);
    let ping_consumer = ping_consumer_vec.pop().unwrap();

    spawn(move || {
        loop {
            // It's possible to allow consumers to mutate each item during processing to communicate
            // with downstream consumers, but that's not implemented yet. For now, the received
            // value isn't echoed back in any way.

            // Initialize to a dummy value, to avoid compile error about capturing a possibly
            // uninitialized variable.
            let mut i = 0;
            ping_consumer.consume(|value: &u64| {
                i = *value;
            });
            // In-band magic value indicates that we should exit
            if u64::MAX == i {
                break;
            }
        }
    });

    let mut i = 0;

    let bench_id = format!("same-thread ping-pong latency ({})", flavour);
    c.bench_function(bench_id.as_str(), |b| {
        iter_latency(b, || {
            ping_publisher.publish(i);
            let i_echo = pong_consumer.take();
            assert_eq!(i, i_echo);
            i += 1;
        })
    });
    ping_publisher.publish(u64::MAX);
}

fn measure_ping_pong_latency_one_ringbuffer_spin(c: &mut Criterion) {
    let w = SpinWaitStrategy;
    measure_ping_pong_latency_one_ringbuffer_generic("spin", c, w);
}

fn measure_ping_pong_latency_one_ringbuffer_yield(c: &mut Criterion) {
    let w = YieldWaitStrategy::new();
    measure_ping_pong_latency_one_ringbuffer_generic("yield", c, w);
}

fn measure_ping_pong_latency_one_ringbuffer_block(c: &mut Criterion) {
    let w = BlockingWaitStrategy::new();
    measure_ping_pong_latency_one_ringbuffer_generic("block", c, w);
}

criterion_group!(
    benches,
    measure_ping_pong_latency_one_ringbuffer_spin,
    measure_ping_pong_latency_one_ringbuffer_yield,
    measure_ping_pong_latency_one_ringbuffer_block,
    measure_ping_pong_latency_two_ringbuffers_spin,
    measure_ping_pong_latency_two_ringbuffers_yield,
    measure_ping_pong_latency_two_ringbuffers_block,
);
criterion_main!(benches);
