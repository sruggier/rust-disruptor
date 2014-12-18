extern crate disruptor;
extern crate time;
extern crate test;

use disruptor::{SinglePublisher, ProcessingWaitStrategy, SpinWaitStrategy, YieldWaitStrategy,
    BlockingWaitStrategy, PipelineInit, Publisher, Consumer, FinalConsumer};
use test::Bencher;
use std::u64;
use std::task::{spawn};

/**
 * Run a two-disruptor ping-pong latency benchmark with the given wait strategy and spawn function.
 *
 * # Arguments
 *
 * * b - the Bencher
 * * w - The wait strategy to use
 */
fn measure_ping_pong_latency_two_ringbuffers_generic<W: ProcessingWaitStrategy>(
    b: &mut Bencher,
    w: W
)
{
    let mut ping_publisher = SinglePublisher::<u64, W>::new(8192, w.clone());
    let ping_consumer = ping_publisher.create_single_consumer_pipeline();
    let mut pong_publisher = SinglePublisher::<u64, W>::new(8192, w.clone());
    let pong_consumer = pong_publisher.create_single_consumer_pipeline();

    spawn(proc() {
        loop {
            // Echo every received value
            let i = ping_consumer.take();
            // In-band magic value indicates that we should exit
            if u64::MAX == i {
                break;
            }
            else {
                pong_publisher.publish(i);
            }
        }
    });

    let mut i = 0;

    b.iter(|| {
        ping_publisher.publish(i);
        let i_echo = pong_consumer.take();
        assert_eq!(i, i_echo);
        i += 1;
    });
    ping_publisher.publish(u64::MAX);
}

#[bench]
fn measure_ping_pong_latency_two_ringbuffers_spin(b: &mut Bencher) {
    let w = SpinWaitStrategy;
    measure_ping_pong_latency_two_ringbuffers_generic(b, w);
}

#[bench]
fn measure_ping_pong_latency_two_ringbuffers_yield(b: &mut Bencher) {
    let w = YieldWaitStrategy::new();
    measure_ping_pong_latency_two_ringbuffers_generic(b, w);
}

#[bench]
fn measure_ping_pong_latency_two_ringbuffers_block(b: &mut Bencher) {
    let w = BlockingWaitStrategy::new();
    measure_ping_pong_latency_two_ringbuffers_generic(b, w);
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
fn measure_ping_pong_latency_one_ringbuffer_generic<W: ProcessingWaitStrategy>(
    b: &mut Bencher,
    w: W
)
{
    let mut ping_publisher = SinglePublisher::<u64, W>::new(8192, w.clone());

    // The second task listens for items from ping_consumer, and the publisher waits for the ping to
    // be processed by listening on pong_consumer before publishing the next item.
    let (mut ping_consumer_vec, pong_consumer) = ping_publisher.create_consumer_pipeline(2);
    let ping_consumer = ping_consumer_vec.pop().take().unwrap();

    spawn(proc() {
        loop {
            // It's possible to allow consumers to mutate each item during processing to communicate
            // with downstream consumers, but that's not implemented yet. For now, the received
            // value isn't echoed back in any way.

            // Initialize to a dummy value, to avoid compile error about capturing a possibly
            // uninitialized variable.
            let mut i = 0;
            ping_consumer.consume( |value: &u64| {
                i = *value;
            });
            // In-band magic value indicates that we should exit
            if u64::MAX == i {
                break;
            }
        }
    });

    let mut i = 0;

    b.iter(|| {
        ping_publisher.publish(i);
        let i_echo = pong_consumer.take();
        assert_eq!(i, i_echo);
        i += 1;
    });
    ping_publisher.publish(u64::MAX);
}

#[bench]
fn measure_ping_pong_latency_one_ringbuffer_spin(b: &mut Bencher) {
    let w = SpinWaitStrategy;
    measure_ping_pong_latency_one_ringbuffer_generic(b, w);
}

#[bench]
fn measure_ping_pong_latency_one_ringbuffer_yield(b: &mut Bencher) {
    let w = YieldWaitStrategy::new();
    measure_ping_pong_latency_one_ringbuffer_generic(b, w);
}

#[bench]
fn measure_ping_pong_latency_one_ringbuffer_block(b: &mut Bencher) {
    let w = BlockingWaitStrategy::new();
    measure_ping_pong_latency_one_ringbuffer_generic(b, w);
}
