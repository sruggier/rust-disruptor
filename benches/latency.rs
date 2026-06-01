use criterion::{
    Bencher, BenchmarkGroup, BenchmarkId, Criterion, criterion_group, criterion_main,
    measurement::WallTime,
};
use disruptor::{
    BlockingWaitStrategy, Consumer, ConsumerMut, PipelineInit, ProcessingWaitStrategy, Publisher,
    SinglePublisher, SpinWaitStrategy, YieldWaitStrategy,
};
use quanta::Instant;
use std::{
    hint::black_box,
    thread::{spawn, yield_now},
    time::Duration,
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

/// Run a two-disruptor ping-pong latency benchmark with the given wait strategy and spawn function.
///
/// # Arguments
///
/// * c - the benchmark manager
/// * w - The wait strategy to use
fn measure_ping_pong_latency_two_ringbuffers_generic<W>(
    flavour: &str,
    g: &mut BenchmarkGroup<WallTime>,
    w: W,
) where
    W: ProcessingWaitStrategy + 'static,
{
    let mut ping_publisher = SinglePublisher::<u64, 8192, W>::new(w.clone());
    let ping_consumer = ping_publisher.create_single_consumer_pipeline();
    let mut pong_publisher = SinglePublisher::<u64, 8192, W>::new(w.clone());
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

    g.bench_function(BenchmarkId::new("two ring buffers", flavour), |b| {
        iter_latency(b, || {
            ping_publisher.publish(i);
            let i_echo = pong_consumer.take();
            assert_eq!(i, i_echo);
            i += 1;
        })
    });
    ping_publisher.publish(u64::MAX);
}

/// Run a one-disruptor ping-pong latency benchmark with the given wait strategy and spawn function.
/// In this version, a single disruptor is used to synchronize the two tasks, which avoids some
/// redundancy.
///
/// # Arguments
///
/// * b - the Bencher
/// * w - The wait strategy to use
fn measure_ping_pong_latency_one_ringbuffer_generic<W>(
    flavour: &str,
    g: &mut BenchmarkGroup<WallTime>,
    w: W,
) where
    W: ProcessingWaitStrategy + 'static,
{
    let mut ping_publisher = SinglePublisher::<u64, 8192, W>::new(w.clone());

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

    g.bench_function(BenchmarkId::new("single ring buffer", flavour), |b| {
        iter_latency(b, || {
            ping_publisher.publish(i);
            let i_echo = pong_consumer.take();
            assert_eq!(i, i_echo);
            i += 1;
        })
    });
    ping_publisher.publish(u64::MAX);
}

fn bench_latency(c: &mut Criterion) {
    let mut latency_group = c.benchmark_group("ping-pong latency");
    latency_group.throughput(criterion::Throughput::Elements(1));
    measure_ping_pong_latency_one_ringbuffer_generic("spin", &mut latency_group, SpinWaitStrategy);
    measure_ping_pong_latency_one_ringbuffer_generic(
        "yield",
        &mut latency_group,
        YieldWaitStrategy::new(),
    );
    measure_ping_pong_latency_one_ringbuffer_generic(
        "block",
        &mut latency_group,
        BlockingWaitStrategy::new(),
    );
    measure_ping_pong_latency_two_ringbuffers_generic("spin", &mut latency_group, SpinWaitStrategy);
    measure_ping_pong_latency_two_ringbuffers_generic(
        "yield",
        &mut latency_group,
        YieldWaitStrategy::new(),
    );
    measure_ping_pong_latency_two_ringbuffers_generic(
        "block",
        &mut latency_group,
        BlockingWaitStrategy::new(),
    );
    latency_group.finish();
}

criterion_group!(benches, bench_latency,);
criterion_main!(benches);
