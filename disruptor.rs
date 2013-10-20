use std::clone::Clone;
use std::cast;
use std::option::{Option};
use std::ptr;
use std::vec;
use std::unstable::sync::UnsafeArc;
use std::unstable::atomics::{AtomicUint,Acquire,Release};

/**
 * Raw pointer to a single nullable element of T. We are going to communicate between tasks by
 * passing objects through a ring buffer. The ring buffer is implemented using std::vec, which
 * requires that its contents are clonable. However, we don't want to impose this limitation on
 * callers, so instead, the buffer will store pointers to Option<T>. The pointers are cloned as
 * needed, but the ring buffer has to handle deallocation of these objects to maintain safety.
 */
struct Slot<T> {
    payload: *mut Option<T>
}

impl<T> Clone for Slot<T> {
    fn clone(&self) -> Slot<T> {
        Slot { payload: self.payload }
    }
}

impl<T> Slot<T> {
    /**
     * Allocates an owned box containing Option<T>, then overrides Rust's memory management by
     * storing it as a raw pointer.
     */
    unsafe fn new() -> Slot<T> {
        let payload: ~Option<T> = ~None;
        let payload_raw: *mut Option<T> = cast::transmute(payload);
        Slot {
            payload: payload_raw
        }
    }

    /**
     * Overwrites the slot's value with `value`.
     *
     * # Safety notes
     *
     * This is safe to call in between new and destroy, but is marked as unsafe because it is still
     * the caller's responsibility to ensure that it is not called after destroy.
     */
    unsafe fn set(&mut self, value: T) {
        let p = self.payload;
        (*p) = Some(value);
    }

    /**
     * Retrieves an immutable reference to the slot's value.
     *
     * # Safety notes
     *
     * It is also the caller's responsibility not to call this after destroy.
     */
    unsafe fn get<'s>(&'s self) -> &'s T {
        let p = self.payload;
        (*p).get_ref()
    }

    /**
     * Deallocates the owned box. This cannot happen automatically, so it is the caller's
     * responsibility to call this at the right time, then avoid dereferencing `payload` after doing
     * so.
     */
    unsafe fn destroy(&mut self) {
        // Deallocate
        let _payload: ~Option<T> = cast::transmute(self.payload);
        self.payload = ptr::mut_null();
    }
}

/**
 * Contains the underlying std::vec, and manages the lifetime of the slots.
 */
struct RingBufferData<T> {
    entries: ~[Slot<T>],
}

impl<T> RingBufferData<T> {
    fn new(size: uint) -> RingBufferData<T> {
        // See Drop below for corresponding slot deallocation
        let buffer = vec::from_fn(size, |_i| { unsafe { Slot::new() } } );
        RingBufferData {
            entries: buffer,
        }
    }
}

#[unsafe_destructor]
impl<T> Drop for RingBufferData<T> {
    fn drop(&mut self) {
        unsafe {
            for entry in self.entries.mut_iter() {
                entry.destroy();
            }
        }
    }
}

/**
 * Values of this object are used as indices into the ring buffer (modulo the buffer size). The
 * current value represents the latest slot that a publisher or consumer is still processing. In
 * other words, a value of 0 means that no slots have been published or consumed, and that 0 is the
 * current slot being processed. A value of 1 means that slot 0 has be released for processing by
 * downstream consumers, while a value of 18 would mean that slots 0-17 are available for
 * processing.
 */
struct SequenceNumber(uint);

/**
 * Represents an initial state where no slots have been published or consumed.
 */
static SEQUENCE_INITIAL: uint = 0;

impl SequenceNumber {
    /// Returns `SEQUENCE_INITIAL` as a `SequenceNumber`.
    fn initial() -> SequenceNumber {
        SequenceNumber(SEQUENCE_INITIAL)
    }

    /**
     * Returns self modulo `buffer_size`, exploiting the assumption that the size will always be a
     * power of two by using a masking operation instead of the modulo operator.
     */
    fn as_index(self, buffer_size: uint) -> uint {
        assert!(buffer_size.population_count() == 1, "buffer_size must be a power of two (received {:?})", buffer_size);
        let index_mask = buffer_size - 1;
        *self & index_mask
    }
}

/**
 * A ring buffer that takes `SequenceNumber` values for get and set operations, performing wrapping
 * automatically. Memory is managed using reference counting.
 *
 * # Safety notes
 *
 * It is the caller's responsibility to avoid data races when reading and writing elements.
 */
struct RingBuffer<T> {
    data: UnsafeArc<RingBufferData<T>>,
}

impl<T: Send> Clone for RingBuffer<T> {
    /// Copy a reference to the original buffer.
    fn clone(&self) -> RingBuffer<T> {
        RingBuffer { data: self.data.clone() }
    }
}

impl<T: Send> RingBuffer<T> {
    /**
     * Constructs a new RingBuffer with a capacity of `size` elements. The size must be a power of
     * two, a property which will be exploited for performance reasons.
     */
    fn new(size: uint) -> RingBuffer<T> {
        assert!(size.population_count() == 1, "RingBuffer size must be a power of two (received {:?})", size);
        let data = RingBufferData::new(size);
        RingBuffer { data: UnsafeArc::new(data) }
    }

    /// Get the size of the underlying buffer.
    fn size(&self) -> uint {
        unsafe {
            (*self.data.get_immut()).entries.len()

        }
    }

    /**
     * Write a value into the ring buffer. The given sequence number is converted into an index into
     * the buffer, and the value is moved in into that element of the buffer.
     */
    unsafe fn set(&mut self, sequence: SequenceNumber, value: T) {
        let d = &mut(*self.data.get());
        let index = sequence.as_index(d.entries.len());
        d.entries[index].set(value);
    }

    /// Get an immutable reference to the value pointed to by `sequence`.
    unsafe fn get<'s>(&'s self, sequence: SequenceNumber) -> &'s T {
        let d = &(*self.data.get_immut());
        let index = sequence.as_index(d.entries.len());
        d.entries[index].get()
    }
}

#[should_fail]
#[test]
fn ring_buffer_size_must_be_power_of_two_7() {
    RingBuffer::<()>::new(7);
}
#[test]
fn ring_buffer_size_must_be_power_of_two_1() {
    RingBuffer::<()>::new(1);
}
#[test]
fn ring_buffer_size_must_be_power_of_two_8() {
    RingBuffer::<()>::new(8);
}

/**
 * Returns how many slots are open between the publisher's sequence and the consumer's sequence,
 * taking into account the effects of wrapping. In this case, the waiting task is a consumer, and
 * the gating task is either a consumer or a publisher. If the gating sequence (in other words, the
 * publisher or upstream consumer) has the same value as the waiting sequence (the consumer), then
 * no slots are available for consumption.
 */
fn calculate_available_consumer(
    gating_sequence: SequenceNumber,
    waiting_sequence: SequenceNumber,
    buffer_size: uint
) -> uint {
    let mut gating = *gating_sequence;
    let waiting = *waiting_sequence;
    // This condition should never happen until wrapping is implemented
    if gating < waiting {
        gating += 2*buffer_size;
    }
    let available = gating - waiting;
    assert!(available <= buffer_size, fmt!("available: %?, gating:%?, waiting: %?", available, gating, waiting));
    available
}

#[test]
fn test_calculate_available_consumer() {
    // Consumer waiting for publisher or earlier consumer
    assert!(1 == calculate_available_consumer(SequenceNumber(13), SequenceNumber(12), 8));
    assert!(1 == calculate_available_consumer(SequenceNumber(1), SequenceNumber(0), 2));
    assert!(2 == calculate_available_consumer(SequenceNumber(2), SequenceNumber(0), 2));

    // Test a hypothetical sequence of states
    assert!(0 == calculate_available_consumer(SequenceNumber(0), SequenceNumber(0), 8));
    assert!(1 == calculate_available_consumer(SequenceNumber(1), SequenceNumber(0), 8));
    assert!(8 == calculate_available_consumer(SequenceNumber(8), SequenceNumber(0), 8));

    // Test wrapping (publisher wraps to 0 at 2*buffer_size)
    assert!(7 == calculate_available_consumer(SequenceNumber(15), SequenceNumber(8), 8));
    assert!(8 == calculate_available_consumer(SequenceNumber(0), SequenceNumber(8), 8));
    assert!(7 == calculate_available_consumer(SequenceNumber(0), SequenceNumber(9), 8));

}

/**
 * Returns how many slots are open between the publisher's sequence and the consumer's sequence,
 * taking into account the effects of wrapping.
 */
fn calculate_available_publisher(
    gating_sequence: SequenceNumber,
    waiting_sequence: SequenceNumber,
    buffer_size: uint
) -> uint {
    let mut available = *gating_sequence + buffer_size - *waiting_sequence;
    if available > buffer_size {
        // Should never happen until wrapping is implemented
        let index_mask = buffer_size - 1;
        available &= index_mask;
    }
    available
}

#[test]
fn test_calculate_available_publisher() {
    // Publisher waiting for consumer
    assert!(8 == calculate_available_publisher(SequenceNumber(17), SequenceNumber(17), 8));
    assert!(1 == calculate_available_publisher(SequenceNumber(5), SequenceNumber(12), 8));
    assert!(0 == calculate_available_publisher(SequenceNumber(5), SequenceNumber(13), 8));
    assert!(1 == calculate_available_publisher(SequenceNumber(6), SequenceNumber(13), 8));

    // Test a few in sequence
    assert!(2 == calculate_available_publisher(SequenceNumber(0), SequenceNumber(0), 2));
    assert!(1 == calculate_available_publisher(SequenceNumber(0), SequenceNumber(1), 2));
    assert!(0 == calculate_available_publisher(SequenceNumber(0), SequenceNumber(2), 2));
    assert!(1 == calculate_available_publisher(SequenceNumber(1), SequenceNumber(2), 2));
    assert!(0 == calculate_available_publisher(SequenceNumber(1), SequenceNumber(3), 2));
    assert!(1 == calculate_available_publisher(SequenceNumber(2), SequenceNumber(3), 2));
    assert!(2 == calculate_available_publisher(SequenceNumber(3), SequenceNumber(3), 2));


}

/**
 * The underlying data referenced by Sequence.
 */
struct SequenceData {
    // Prevent false sharing by padding either side of the value with 60 bytes of data.
    // TODO: If uint is 64 bits wide, this is more padding than needed.
    padding1: [u32, ..15],
    /// The published value of the sequence, visible to waiting consumers.
    value: AtomicUint,
    /// We can avoid atomic operations by using this cached value whenever possible.
    private_value: uint,
    padding2: [u32, ..14],
}

impl SequenceData {
    fn new(initial_value: uint) -> SequenceData {
        SequenceData {
            padding1: [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
            value: AtomicUint::new(initial_value),
            private_value: initial_value,
            padding2: [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
        }
    }
}

/**
 * Mutable reference to an atomic uint. Returns values as SequenceNumber to disambiguate from
 * indices and other uint values. Memory is managed via reference counting.
 */
struct Sequence {
    value_arc: UnsafeArc<SequenceData>
}

impl Sequence {
    /// Allocates a new sequence.
    fn new() -> Sequence {
        Sequence {
            value_arc: UnsafeArc::new(SequenceData::new(SEQUENCE_INITIAL)),
        }
    }

    /// Get a reference to the underlying data. Internal helper.
    fn get_data<'s>(&'s self) -> &'s SequenceData {
        unsafe {
            &(*self.value_arc.get_immut())
        }
    }

    /// Get a mutable reference to the underlying data. Internal helper.
    fn get_mut_data<'s>(&'s mut self) -> &'s mut SequenceData {
        unsafe {
            &mut(*self.value_arc.get())
        }
    }

    /// See SequenceReader's get method
    fn get(&self) -> SequenceNumber {
        SequenceNumber(self.get_data().value.load(Acquire))
    }

    /**
     * Gets the internally cached value of the Sequence. This should only be called from the task
     * that owns the sequence number (in other words, the only task that writes to the sequence
     * number)
     */
    fn get_owned(&self) -> SequenceNumber {
        SequenceNumber(self.get_data().private_value)
    }

    /**
     * Return an immutable reference to the same underlying sequence number.
     */
    fn clone_immut(&self) -> SequenceReader {
        SequenceReader {
            sequence: Sequence {
                value_arc: self.value_arc.clone()
            }
        }
    }

    /**
     * Add n to the sequence and ensure that other writes that have taken place before the call
     * are visible other tasks before this write.
     */
    fn advance(&mut self, n: uint) {
        let d = self.get_mut_data();
        d.private_value += n;
        d.value.store(d.private_value, Release);
    }
}

/**
 * Immutable reference to a sequence. Can be safely given to other tasks. Reads with acquire
 * semantics.
 */
struct SequenceReader {
    sequence: Sequence,
}

impl SequenceReader {
    /**
     * Gets the value of the sequence, using acquire semantics. For use by publishers/consumers to
     * confirm that slots have been released by the task(s) ahead of them in the pipeline.
     */
    pub fn get(&self) -> SequenceNumber { self.sequence.get() }
    /// Get another reference to the sequence.
    pub fn clone_immut(&self) -> SequenceReader { self.sequence.clone_immut() }
}

#[test]
fn test_sequencereader() {
    let mut sequence =  Sequence::new();
    let reader = sequence.clone_immut();
    assert!(0 == *reader.get());
    sequence.advance(1);
    assert!(1 == *reader.get());
    sequence.advance(11);
    assert!(12 == *reader.get());
}

/// Helps consumers wait on upstream dependencies.
trait ProcessingWaitStrategy : PublishingWaitStrategy {
    /**
     * Waits for `cursor` to release the next `n` slots. For strategies that block, only the
     * publisher will attempt to wake the task. Therefore, the publisher's `cursor` is needed so
     * that once the publisher has advanced sufficiently, the task will stop blocking and busy-wait
     * on its immediate dependencies for the event to become available for processing. Once the
     * publisher has released the necessary slots, the rest of the pipeline should release them in a
     * relatively bounded amount of time, so it's probably worth wasting some CPU time to achieve
     * lower latency.
     *
     * # TODO
     *
     * Wait strategies for oversubscribed situations in which there are more tasks publishing
     * and consuming than the number of available CPU cores. It would be good to try to design a
     * solution where we work with the task scheduler to switch to another task without involving
     * the kernel, if possible.
     */
    fn wait_for_publisher(
        &self,
        n: uint,
        waiting_sequence: SequenceNumber,
        cursor: &SequenceReader,
        cached_cursor_value: SequenceNumber,
        buffer_size: uint
    ) -> SequenceNumber;
}
/**
 * Helps the publisher wait to avoid overwriting values that are still being consumed.
 */
trait PublishingWaitStrategy : Clone {
    /**
     * Wait for upstream consumers to finish processing items that have already been published.
     */
    fn wait_for_consumers(
        &self,
        n: uint,
        waiting_sequence: SequenceNumber,
        dependencies: &[SequenceReader],
        buffer_size: uint,
        calculate_available: &fn(gating_sequence: SequenceNumber, waiting_sequence: SequenceNumber, batch_size: uint) -> uint
    );

    /**
     * Wakes up any consumers that have blocked waiting for new items to be published.
     */
    fn notifyAllWaiters(&mut self);
}

/**
 * Waits using simple busy waiting.
 *
 * # Safety notes
 *
 * Using this strategy can result in livelock when used with tasks spawned using default scheduler
 * options. Ensure all publishers and consumers are on separate OS threads when using this.
 */
#[deriving(Clone)]
pub struct SpinWaitStrategy;

impl ProcessingWaitStrategy for SpinWaitStrategy {
    fn wait_for_publisher(
        &self,
        n: uint,
        waiting_sequence: SequenceNumber,
        cursor: &SequenceReader,
        cached_cursor_value: SequenceNumber,
        buffer_size: uint
    ) -> SequenceNumber {
        let mut available = calculate_available_consumer(cached_cursor_value, waiting_sequence, buffer_size);
        let mut cursor_value = cached_cursor_value;
        while n > available {
            cursor_value = cursor.get();
            available = calculate_available_consumer(cursor_value, waiting_sequence, buffer_size);
        }

        cursor_value
    }
}
impl PublishingWaitStrategy for SpinWaitStrategy {
    fn wait_for_consumers(
        &self,
        n: uint,
        waiting_sequence: SequenceNumber,
        dependencies: &[SequenceReader],
        buffer_size: uint,
        calculate_available: &fn(gating_sequence: SequenceNumber, waiting_sequence: SequenceNumber, batch_size: uint) -> uint
    ) {
        // Iterate over each dependency in turn, waiting for them to release the next `n` items
        for consumer_sequence in dependencies.iter() {
            while n > calculate_available(consumer_sequence.get(), waiting_sequence, buffer_size) {
                // busy wait
            }
        }
    }

    fn notifyAllWaiters(&mut self) {
    }
}

/**
 * Responsible for ensuring that the caller does not proceed until one or more dependent sequences
 * have finished working with the subsequent slots.
 */
trait SequenceBarrier {
    /**
     * Get the current value of the sequence associated with this SequenceBarrier.
     */
    fn get_current(&self) -> SequenceNumber;

    /**
     * Wait for a single slot to be available.
     */
    fn next(&mut self) {
        self.nextN(1)
    }

    /**
     * Wait for N slots to be available.
     *
     * # Safety notes
     *
     * Note that if N is greater than the size of the RingBuffer minus the total number of slots the
     * rest of the pipeline is waiting for, then this function may deadlock. A size of 1 should
     * always be safe. Alternatively, increase the size of the buffer to support the desired amount
     * of batching.
     */
    fn nextN(&mut self, batch_size: uint);

    /**
     * Release a single slot for downstream consumers.
     */
    fn release(&mut self) {
        self.releaseN(1);
    }

    /**
     * Release n slots for downstream consumers.
     */
    fn releaseN(&mut self, batch_size: uint);
}

/**
 * Implements `SequenceBarrier` for publishers in situations where there's only one concurrent
 * publisher.
 */
struct SinglePublisherSequenceBarrier<W> {
    sequence: Sequence,
    dependencies: ~[SequenceReader],
    wait_strategy: W,
    buffer_size: uint,
}

impl<W: PublishingWaitStrategy> SinglePublisherSequenceBarrier<W> {
    fn new(
        dependencies: ~[SequenceReader],
        wait_strategy: W,
        buffer_size: uint
    ) -> SinglePublisherSequenceBarrier<W> {
        SinglePublisherSequenceBarrier {
            sequence: Sequence::new(),
            dependencies: dependencies,
            wait_strategy: wait_strategy,
            buffer_size: buffer_size,
        }
    }

    fn advance(&mut self, batch_size: uint) {
        self.sequence.advance(batch_size);
    }

    /**
     * Assign a new set of dependencies to this barrier.
     *
     * # TODO
     *
     * After settling on a design for concurrent producers and perhaps concurrent consumers,
     * redesign dependencies to be immutable after the object is constructed.
     */
    fn setDependencies(&mut self, dependencies: ~[SequenceReader]) {
        self.dependencies = dependencies;
    }
}

impl<W: PublishingWaitStrategy> SequenceBarrier for SinglePublisherSequenceBarrier<W> {
    fn get_current(&self) -> SequenceNumber {
        self.sequence.get_owned()
    }

    fn nextN(&mut self, batch_size: uint) {
        let current_sequence = self.sequence.get_owned();
        self.wait_strategy.wait_for_consumers(batch_size, current_sequence, self.dependencies, self.buffer_size, calculate_available_publisher)
    }

    fn releaseN(&mut self, batch_size: uint) {
        self.advance(batch_size);
        self.wait_strategy.notifyAllWaiters();
    }
}

/**
 * Implements `SequenceBarrier` for consumers. This implementation supports multiple concurrent
 * consumers, but all consumers will process all events. This is unsuitable for when a
 * load-balancing arrangement is desired.
 */
struct SingleConsumerSequenceBarrier<W> {
    sb: SinglePublisherSequenceBarrier<W>,
    /**
     * A reference to the publisher's sequence.
     */
    cursor: SequenceReader,
    /**
     * Contains the last value retrieved from the publisher's task. Caching allows for batching in
     * cases where the upstream dependency has pulled ahead.
     */
    cached_cursor_value: SequenceNumber,
}

impl<W: ProcessingWaitStrategy> SingleConsumerSequenceBarrier<W> {
    fn new(
        cursor: SequenceReader,
        dependencies: ~[SequenceReader],
        wait_strategy: W,
        buffer_size: uint
    ) -> SingleConsumerSequenceBarrier<W> {
        SingleConsumerSequenceBarrier {
            sb: SinglePublisherSequenceBarrier::new(
                dependencies,
                wait_strategy,
                buffer_size
            ),
            cursor: cursor,
            cached_cursor_value: SequenceNumber::initial(),
        }
    }
}

impl<W: ProcessingWaitStrategy> SequenceBarrier for SingleConsumerSequenceBarrier<W> {
    fn get_current(&self) -> SequenceNumber {
        self.sb.get_current()
    }

    fn nextN(&mut self, batch_size: uint) {
        let current_sequence = self.get_current();
        self.cached_cursor_value = self.sb.wait_strategy.wait_for_publisher(batch_size, current_sequence, &self.cursor, self.cached_cursor_value, self.sb.buffer_size);
        self.sb.wait_strategy.wait_for_consumers(batch_size, current_sequence, self.sb.dependencies, self.sb.buffer_size, calculate_available_consumer);
    }

    fn releaseN(&mut self, batch_size: uint) {
        self.sb.advance(batch_size);
    }
}

/**
 * Allows callers to wire up dependencies, then send values down the pipeline
 * of dependent consumers.
 */
struct SinglePublisher<T, W> {
    rb: RingBuffer<T>,
    sequence_barrier: SinglePublisherSequenceBarrier<W>,
}

impl<T: Send, W: ProcessingWaitStrategy> SinglePublisher<T, W> {
    /**
     * Constructs a new RingBuffer<T> with _size_ elements and wraps it into a
     * new SinglePublisher<T> object.
     */
    pub fn new(size: uint, wait_strategy: W) -> SinglePublisher<T, W> {
        SinglePublisher {
            rb: RingBuffer::<T>::new(size),
            sequence_barrier: SinglePublisherSequenceBarrier::new(~[], wait_strategy, size),
        }
    }

    /**
     * Create a chain of dependent consumers, and then add the final
     * consumer(s) in the chain as a dependency of the publisher.
     *
     * TODO: take a list of uint to support parallel consumers. Need to pick a convenient return
     * type. Possible candidates:
     *  - List of lists
     *  - List of enum (single or parallel)
     *  - ?
     */
    pub fn create_consumer_chain(
        &mut self,
        count_consumers: uint
    ) -> ~[SingleConsumer<T, W>] {
        assert!(self.sequence_barrier.dependencies.len() == 0, "The create_consumer_chain method can only be called once.");

        let mut consumers = vec::with_capacity::<SingleConsumer<T, W>>(count_consumers);

        // Create each stage in the chain, adding the previous stage as a gating dependency

        // The first stage consumers wait on the publisher's sequence, which is provided to all
        // consumers regardless of their position in the pipeline. It's not necessary to provide a
        // second reference to the same sequence, so an empty array is provided instead.
        let mut dependencies = ~[];
        for _ in range(0, count_consumers) {
            let c = SingleConsumer::<T, W>::new(
                self.rb.clone(),
                self.sequence_barrier.sequence.clone_immut(),
                dependencies,
                self.sequence_barrier.wait_strategy.clone()
            );
            dependencies = ~[c.sequence_barrier.sb.sequence.clone_immut()];
            consumers.push(c);
        }
        self.sequence_barrier.setDependencies(dependencies);
        consumers
    }

    pub fn publish(&self, value: T) {
        unsafe {
            // FIXME #5372
            let self_mut = cast::transmute_mut(self);
            let begin: SequenceNumber = self.sequence_barrier.get_current();
            // Wait for available slot
            self_mut.sequence_barrier.next();
                self_mut.rb.set(begin, value);
            // Make the item available to downstream consumers
            self_mut.sequence_barrier.release();
        }
    }
}

/**
 * Allows callers to retrieve values from upstream tasks in the pipeline.
 */
struct SingleConsumer<T, W> {
    rb: RingBuffer<T>,
    sequence_barrier: SingleConsumerSequenceBarrier<W>,
}

impl<T: Send, W: ProcessingWaitStrategy> SingleConsumer<T, W> {
    fn new(
        rb: RingBuffer<T>,
        cursor: SequenceReader,
        dependencies: ~[SequenceReader],
        wait_strategy: W
    ) -> SingleConsumer<T, W> {
        let size = rb.size();
        SingleConsumer {
            rb: rb,
            sequence_barrier: SingleConsumerSequenceBarrier::new(cursor, dependencies, wait_strategy, size)
        }
    }

    /**
     * Waits for a single item to become available, then calls the given function to process the
     * value.
      */
    pub fn consume(&self, consume_callback: &fn(value: &T)) {
        unsafe {
            // FIXME #5372
            let self_mut = cast::transmute_mut(self);
            self_mut.sequence_barrier.next();
            consume_callback(self.rb.get(self.sequence_barrier.get_current()));
            self_mut.sequence_barrier.release();

        }
    }
}

#[cfg(test)]
mod single_publisher_tests {
    use super::{SinglePublisher, SpinWaitStrategy};

    #[test]
    fn send_single_value() {
        let mut publisher = SinglePublisher::<int, SpinWaitStrategy>::new(1, SpinWaitStrategy);
        let consumer = publisher.create_consumer_chain(1)[0];
        publisher.publish(1);
        consumer.consume(|value: &int| {
            assert!(*value == 1);
        });
    }

    // TODO: test that dependencies hold true by setting up a chain, grabbing a list of timestamps
    // within each task, and then verifying them after for a happens-before relationship. It's not
    // foolproof, but better than nothing.
}
