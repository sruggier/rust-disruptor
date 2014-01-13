// Copyright 2013 Simon Ruggier.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.
use extra::sync::{Mutex};
use std::clone::Clone;
use std::cast;
use std::cmp;
use std::fmt;
use std::option::{Option};
use std::ptr;
use std::task;
use std::vec;
use std::uint;
use std::unstable::sync::UnsafeArc;
use std::unstable::atomics::{AtomicUint,Acquire,Release,AtomicBool,AcqRel};

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
     * Moves the value out of the slot and returns it.
     *
     * # Failure
     *
     * When called a second time, this function doubles as a convenient way to end your task.
     *
     * # Safety notes
     *
     * It's the caller's responsibility not to call this after destroy.
     */
    unsafe fn take(&mut self) -> T {
        (*self.payload).take_unwrap()
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

    /**
     * Write a value into the ring buffer. The given sequence number is converted into an index into
     * the buffer, and the value is moved in into that element of the buffer.
     */
    fn set(&mut self, sequence: SequenceNumber, value: T) {
        let index = sequence.as_index(self.entries.len());
        // We're guaranteed not to have called destroy during the lifetime of this type, so it's safe
        // to call set and get.
        unsafe {
            self.entries[index].set(value);
        }
    }

    /// Get the size of the underlying buffer.
    fn size(&self) -> uint {
        self.entries.len()
    }

    /// Get an immutable reference to the value pointed to by `sequence`.
    fn get<'s>(&'s self, sequence: SequenceNumber) -> &'s T {
        let index = sequence.as_index(self.size());
        unsafe {
            self.entries[index].get()
        }
    }

    /**
     * Take the value pointed to by `sequence`, moving it out of the RingBuffer.
     *
     * # Failure
     *
     * This function should only be called once for a given sequence value.
     */
    fn take(&mut self, sequence: SequenceNumber) -> T {
        let index = sequence.as_index(self.size());
        unsafe {
            self.entries[index].take()
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
 * current slot being processed. A value of 1 means that slot 0 has been released for processing by
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
        // assert!(buffer_size.population_count() == 1, "buffer_size must be a power of two (received {:?})", buffer_size);
        let index_mask = buffer_size - 1;
        *self & index_mask
    }
}

/// UnsafeArc, but with unchecked versions of the get and get_immut functions. The use of atomic
/// operations in those functions is a significant slowdown.
///
/// FIXME: remove this if/when UnsafeArc exposes similar functions.
struct UncheckedUnsafeArc<T> {
    arc: UnsafeArc<T>,
    data: *mut T,
}

impl<T: Send> UncheckedUnsafeArc<T> {
    fn new(data: T) -> UncheckedUnsafeArc<T> {
        let arc = UnsafeArc::new(data);
        let data = arc.get();
        UncheckedUnsafeArc {
            arc: arc,
            data: data,
        }
    }

    unsafe fn get<'s>(&'s mut self) -> &'s mut T {
        &mut *self.data
    }

    unsafe fn get_immut<'s>(&'s self) -> &'s T {
        &*self.data
    }
}

impl<T: Send> Clone for UncheckedUnsafeArc<T> {
    fn clone(&self) -> UncheckedUnsafeArc<T> {
        UncheckedUnsafeArc {
            arc: self.arc.clone(),
            data: self.data,
        }
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
    data: UncheckedUnsafeArc<RingBufferData<T>>,
}

impl<T: Send> Clone for RingBuffer<T> {
    /// Copy a reference to the original buffer.
    fn clone(&self) -> RingBuffer<T> {
        RingBuffer { data: self.data.clone() }
    }
}

/**
 * Allows for different ring buffer implementations to be used by the higher level types in this
 * module.
 */
trait RingBufferTrait<T> : Clone + Send {
    /**
     * Constructs a new RingBuffer with a capacity of `size` elements. The size must be a power of
     * two, a property which will be exploited for performance reasons.
     */
    fn new(size: uint) -> Self;

    /// See `RingBufferData::size`
    fn size(&self) -> uint;

    /**
     * See `RingBufferData::set`
     *
     * # Safety notes
     *
     * It's the caller's responsibility to avoid data races, so this function is unsafe.
     */
    unsafe fn set(&mut self, sequence: SequenceNumber, value: T);

    /**
     * See `RingBufferData::get`. Unsafe: allows data races.
     *
     * Mutable to facilitate transparent transitions to larger buffers.
     */
    unsafe fn get<'s>(&'s mut self, sequence: SequenceNumber) -> &'s T;

    /// See `RingBufferData::take`. Unsafe: allows data races.
    unsafe fn take(&mut self, sequence: SequenceNumber) -> T;
}

impl<T: Send> RingBuffer<T> {
    fn new(size: uint) -> RingBuffer<T> {
        RingBufferTrait::new(size)
    }
}

impl<T: Send> RingBufferTrait<T> for RingBuffer<T> {
    fn new(size: uint) -> RingBuffer<T> {
        assert!(size.population_count() == 1, "RingBuffer size must be a power of two (received {:?})", size);
        let data = RingBufferData::new(size);
        RingBuffer { data: UncheckedUnsafeArc::new(data) }
    }

    fn size(&self) -> uint {
        unsafe {
            self.data.get_immut().size()
        }
    }

    unsafe fn set(&mut self, sequence: SequenceNumber, value: T) {
        let d = self.data.get();
        d.set(sequence, value);
    }

    unsafe fn get<'s>(&'s mut self, sequence: SequenceNumber) -> &'s T {
        let d = self.data.get_immut();
        d.get(sequence)
    }

    unsafe fn take(&mut self, sequence: SequenceNumber) -> T {
        let d = self.data.get();
        d.take(sequence)
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
    // Handle wrapping
    if gating < waiting {
        gating += 2*buffer_size;
    }
    let available = gating - waiting;
    assert!(available <= buffer_size, "available: {:?}, gating: {:?}, waiting: {:?}", available, gating, waiting);
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
    // Handle wrapping
    if available > buffer_size {
        // In this case, we know that the value of available is exactly 2*buffer_size more than it
        // should be. Mask out the 2*buffer_size extra slots, taking advantage of the fact that
        // buffer_size is a power of 2.
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
 * Used on either side of Uint values to fill the remainder of a cache line, to avoid false sharing
 * with other threads.
 */
struct UintPadding {
    padding: [u8, ..uint_padding_size]
}

// This is calculated to be (cache line size - uint size), in bytes
#[cfg(target_word_size = "32")] static uint_padding_size: uint = 60;
#[cfg(target_word_size = "64")] static uint_padding_size: uint = 56;

impl UintPadding {
    fn new() -> UintPadding {
        UintPadding { padding: [0, ..uint_padding_size] }
    }
}

/**
 * The underlying data referenced by Sequence.
 */
struct SequenceData {
    // Prevent false sharing by padding either side of the value
    padding1: UintPadding,
    /// The published value of the sequence, visible to waiting consumers.
    value: AtomicUint,
    padding2: UintPadding,
    /// We can avoid atomic operations by using this cached value whenever possible.
    private_value: uint,
    padding3: UintPadding,
}

impl SequenceData {
    fn new(initial_value: uint) -> SequenceData {
        SequenceData {
            padding1: UintPadding::new(),
            value: AtomicUint::new(initial_value),
            padding2: UintPadding::new(),
            private_value: initial_value,
            padding3: UintPadding::new(),
        }
    }
}

/**
 * Mutable reference to an atomic uint. Returns values as SequenceNumber to disambiguate from
 * indices and other uint values. Memory is managed via reference counting.
 */
struct Sequence {
    value_arc: UncheckedUnsafeArc<SequenceData>
}

impl Sequence {
    /// Allocates a new sequence.
    fn new() -> Sequence {
        Sequence {
            value_arc: UncheckedUnsafeArc::new(SequenceData::new(SEQUENCE_INITIAL)),
        }
    }

    /// See SequenceReader's get method
    fn get(&self) -> SequenceNumber {
        unsafe {
            SequenceNumber(self.value_arc.get_immut().value.load(Acquire))
        }
    }

    /**
     * Gets the internally cached value of the Sequence. This should only be called from the task
     * that owns the sequence number (in other words, the only task that writes to the sequence
     * number)
     */
    fn get_owned(&self) -> SequenceNumber {
        unsafe {
            SequenceNumber(self.value_arc.get_immut().private_value)
        }
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
     * Add n to the cached, private version of the sequence, without making the new value visible to
     * other threads.
     *
     * To avoid overflow when uint is 32 bits wide, this function also wraps the sequence number
     * around when it reaches 2*buffer_size. This results in two easily distinguishable states for
     * the availability calculations to handle. Consumer sequences are normally behind gating
     * sequences. However, the gating sequence will wrap first, remain behind for buffer_size slots,
     * and then the waiting sequence will wrap. The publisher is normally ahead of the sequence it
     * depends on, but after wrapping, it will be temporarily behind the gating sequence.
     */
    fn advance(&mut self, n: uint, buffer_size: uint) {
        unsafe {
            let d = self.value_arc.get();
            d.private_value += n;
            // Given that buffer_size is a power of two, wrap by masking out the high bits. This
            // operation is a noop if the value is less than 2*buffer_size, so it's unnecessary to
            // check before wrapping.
            let wrap_mask = 2*buffer_size - 1;
            d.private_value &= wrap_mask;
        }
    }

    /**
     *  Publishes the private sequence value to other threads, along with any other writes (for
     *  example, to the corresponding item in the ring buffer) that have taken place before the
     *  call.
     */
    fn flush(&mut self) {
        unsafe {
            let d = self.value_arc.get();
            d.value.store(d.private_value, Release);
        }
    }

    /**
     * Advance, then immediately make the change visible to other threads.
     */
    fn advance_and_flush(&mut self, n: uint, buffer_size: uint) {
        self.advance(n, buffer_size);
        self.flush();
    }
}

/// Ensure sequences correctly handle buffer sizes of 2^(uint::bits-1).
#[test]
fn test_sequence_overflow() {
    // The maximum buffer size is half of 2^(uint::bits) (for example, 2^31), and uint::max_value
    // is 2*buffer_size - 1. The sequence will wrap to 0 at 2*buffer_size.
    let max_buffer_size = 1 << (uint::bits - 1);

    let mut s = Sequence::new();
    assert_eq!(*s.get(), SEQUENCE_INITIAL);

    // Add 1
    s.advance_and_flush(1, max_buffer_size);
    let incremented_value = s.get();
    assert_eq!(*incremented_value, SEQUENCE_INITIAL + 1);

    // Advance to max_buffer_size
    s.advance_and_flush(max_buffer_size - *incremented_value, max_buffer_size);
    assert_eq!(*s.get(), max_buffer_size);

    // Overflow to 2*max_buffer_size + 1 and confirm expected result (1)
    s.advance_and_flush(max_buffer_size + 1, max_buffer_size);
    assert_eq!(*s.get(), 1);
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
    // For the purposes of this test, it doessn't matter what the buffer size is, as long as it's
    // larger than the tested sequence numbers
    let buffer_size = 8192;

    let mut sequence =  Sequence::new();
    let reader = sequence.clone_immut();
    assert!(0 == *reader.get());
    sequence.advance_and_flush(1, buffer_size);
    assert!(1 == *reader.get());
    sequence.advance_and_flush(11, buffer_size);
    assert!(12 == *reader.get());
}

/// Helps consumers wait on upstream dependencies.
pub trait ProcessingWaitStrategy : PublishingWaitStrategy {
    /**
     * Wait for `cursor` to release the next `n` slots, then return the actual number of available
     * slots, which may be greater than `n`.
     *
     * For strategies that block, only the publisher will attempt to wake the task. Therefore, the
     * publisher's `cursor` is needed so that once the publisher has advanced sufficiently, the task
     * will stop blocking and busy-wait on its immediate dependencies for the event to become
     * available for processing. Once the publisher has released the necessary slots, the rest of
     * the pipeline should release them in a relatively bounded amount of time, so it's probably
     * worth wasting some CPU time to achieve lower latency.
     *
     * # TODO
     *
     * Wait strategies for oversubscribed situations in which there are more tasks publishing
     * and consuming than the number of available CPU cores. It would be good to try to design a
     * solution where we work with the task scheduler to switch to another task without involving
     * the kernel, if possible.
     */
    fn wait_for_publisher(
        &mut self,
        n: uint,
        waiting_sequence: SequenceNumber,
        cursor: &SequenceReader,
        buffer_size: uint
    ) -> uint;
}

/**
 * Helps the publisher wait to avoid overwriting values that are still being consumed.
 */
pub trait PublishingWaitStrategy : Clone + Send {
    /**
     * Wait for upstream consumers to finish processing items that have already been published, then
     * returns the actual number of available items, which may be greater than n. Returns
     * uint::max_value if there are no dependencies.
     */
    fn wait_for_consumers(
        &self,
        n: uint,
        waiting_sequence: SequenceNumber,
        dependencies: &[SequenceReader],
        buffer_size: uint,
        calculate_available: &fn(gating_sequence: SequenceNumber, waiting_sequence: SequenceNumber, batch_size: uint) -> uint
    ) -> uint;

    /**
     * Wakes up any consumers that have blocked waiting for new items to be published.
     *
     * # Safety notes
     *
     * This must be called only after signalling that the slot is published, or it will not always
     * work, and consumers waiting using a blocking wait strategy may sleep indefinitely (until a
     * second item is published).
     */
    fn notifyAllWaiters(&mut self);
}

/**
 * Given a list of dependencies, retrieves the current value of each and returns the minimum number
 * of available items out of all the dependencies.
 */
fn calculate_available_list(
    waiting_sequence: SequenceNumber,
    dependencies: &[SequenceReader],
    buffer_size: uint,
    calculate_available: & &fn(gating_sequence: SequenceNumber, waiting_sequence: SequenceNumber, batch_size: uint) -> uint
) -> uint {
    let mut available = uint::max_value;
    for consumer_sequence in dependencies.iter() {
        let a = (*calculate_available)(consumer_sequence.get(), waiting_sequence, buffer_size);
        available = cmp::min(available, a);
    }
    available
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
        &mut self,
        n: uint,
        waiting_sequence: SequenceNumber,
        cursor: &SequenceReader,
        buffer_size: uint
    ) -> uint {
        let mut available = calculate_available_consumer(cursor.get(), waiting_sequence, buffer_size);
        while n > available {
            // busy wait
            available = calculate_available_consumer(cursor.get(), waiting_sequence, buffer_size);
        }

        available
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
    ) -> uint {
        let mut available = 0;
        while available < n {
            // busy wait
            available = calculate_available_list(waiting_sequence, dependencies, buffer_size,
                &calculate_available);
        }
        available
    }

    fn notifyAllWaiters(&mut self) {
    }
}

impl fmt::Default for SpinWaitStrategy {
    fn fmt(_obj: &SpinWaitStrategy, f: &mut fmt::Formatter) {
        write!(f.buf, "disruptor::SpinWaitStrategy");
    }
}

/**
 * Spin on a consumer gating sequence until either the desired number of elements becomes available,
 * or a maximum number of retries is reached.
 *
 * # Return value
 *
 * The number of available items, which may be less than `n` if the maximum amount of tries was
 * reached.
 */
fn spin_for_consumer_retries(
    n: uint,
    waiting_sequence: SequenceNumber,
    dependencies: &[SequenceReader],
    buffer_size: uint,
    calculate_available: & &fn(gating_sequence: SequenceNumber, waiting_sequence: SequenceNumber, batch_size: uint) -> uint,
    max_tries: uint
) -> uint {
    let mut tries = 0;
    let mut available = 0;
    while available < n && tries < max_tries {
        // busy wait
        available = calculate_available_list(waiting_sequence, dependencies, buffer_size,
            calculate_available);
        tries += 1;
    }
    available
}

/**
 * Spin on a publisher gating sequence until either the desired number of elements becomes available,
 * or a maximum number of retries is reached.
 *
 * # Return value
 *
 * The number of available items, which may be less than `n` if the maximum amount of tries was
 * reached.
 */
fn spin_for_publisher_retries(
    n: uint,
    waiting_sequence: SequenceNumber,
    cursor: &SequenceReader,
    buffer_size: uint,
    max_tries: uint
) -> uint {
    let mut tries = 0;
    let mut available = 0;
    while n > available && tries < max_tries {
        // busy wait
        available = calculate_available_consumer(cursor.get(), waiting_sequence, buffer_size);
        tries += 1;
    }
    available
}

pub static default_max_spin_tries_publisher: uint = 2500;
pub static default_max_spin_tries_consumer: uint = 2500;

/**
 * A wait strategy for use cases where high throughput and low latency are a priority, but it is
 * also desirable to avoid starving other tasks, such as when there are more tasks than CPU cores.
 * Spins for a small number of retries, then yields to other tasks repeatedly until enough items are
 * released. This will almost always be a better choice than SpinWaitStrategy, except in cases where
 * latency is paramount, and the caller has taken steps to pin the publisher and consumers to their
 * own threads, or even cores.
 */
struct YieldWaitStrategy {
    max_spin_tries_publisher: uint,
    max_spin_tries_consumer: uint,
}

impl YieldWaitStrategy {
    /**
     * Create a YieldWaitStrategy that will spin for the default number of times before yielding.
     */
    pub fn new() -> YieldWaitStrategy {
        YieldWaitStrategy::new_with_retry_count(
            default_max_spin_tries_publisher,
            default_max_spin_tries_consumer
        )
    }

    /**
     * Create a YieldWaitStrategy, explicitly specifying how many times to spin before
     * transitioning to a yielding strategy.
     *
     * # Arguments
     *
     * The two arguments represent the maximum number of times to spin while waiting for the
     * publisher or other consumers. This is a tradeoff: one gains lower latency and increased
     * throughput, at the expense of wasted CPU cycles.  When the CPU is oversubscribed, though,
     * more retries could actually reduce throughput. The increased power usage is also undesirable
     * in general. The ideal value depends on how important reduced latency and/or increased
     * throughput are to a given use case, how frequently items are published, and how quickly
     * consumers process new items.
     */
    pub fn new_with_retry_count(
        max_spin_tries_publisher: uint,
        max_spin_tries_consumer: uint
    ) -> YieldWaitStrategy {
        YieldWaitStrategy {
            max_spin_tries_publisher: max_spin_tries_publisher,
            max_spin_tries_consumer: max_spin_tries_consumer,
        }
    }
}

impl Clone for YieldWaitStrategy {
    fn clone(&self) -> YieldWaitStrategy {
        YieldWaitStrategy::new_with_retry_count(
            self.max_spin_tries_publisher, self.max_spin_tries_consumer)
    }
}

impl PublishingWaitStrategy for YieldWaitStrategy {
    fn wait_for_consumers(
        &self,
        n: uint,
        waiting_sequence: SequenceNumber,
        dependencies: &[SequenceReader],
        buffer_size: uint,
        calculate_available: &fn(
            gating_sequence: SequenceNumber,
            waiting_sequence: SequenceNumber,
            batch_size: uint
        ) -> uint
    ) -> uint {
        let mut available = spin_for_consumer_retries(n, waiting_sequence, dependencies, buffer_size,
            &calculate_available, self.max_spin_tries_consumer);

        if available >= n {
            return available;
        }

        while n > available {
            available = calculate_available_list(waiting_sequence, dependencies, buffer_size,
                &calculate_available);
            task::deschedule();
        }

        available
    }

    fn notifyAllWaiters(&mut self) {
    }
}

impl ProcessingWaitStrategy for YieldWaitStrategy {
    fn wait_for_publisher(
        &mut self,
        n: uint,
        waiting_sequence: SequenceNumber,
        cursor: &SequenceReader,
        buffer_size: uint
    ) -> uint {
        let mut available = spin_for_publisher_retries(n, waiting_sequence, cursor, buffer_size,
            self.max_spin_tries_publisher);

        if available >= n {
            return available;
        }

        while available < n {
            available = calculate_available_consumer(cursor.get(), waiting_sequence, buffer_size);
            task::deschedule();
        }

        available
    }
}

impl fmt::Default for YieldWaitStrategy {
    fn fmt(obj: &YieldWaitStrategy, f: &mut fmt::Formatter) {
        write!(f.buf,
            "disruptor::YieldWaitStrategy\\{p: {}, c: {}\\}",
            obj.max_spin_tries_publisher,
            obj.max_spin_tries_consumer
        );
    }
}

/**
 * Spins for a short time, then sleeps on a wait condition until the publisher signals. This comes
 * at a cost, however: the publisher has to perform an extra read-modify-write operation on a shared
 * atomic variable every time it publishes new items. The operation should be uncontended, unless it
 * happens at the same time that a waiter is about to fall asleep. See below for details and a proof
 * of correctness.
 *
 * # Design issues
 *
 * When a waiting task goes to sleep, it cannot sleep without a timeout unless it is certain that
 * the publisher will wake it up when the next slot is published. The conventional solution to this
 * problem would be to have the publisher acquire a lock after every publish, to guarantee that
 * waiting consumers are woken up immediately. This would impose a prohibitive performance penalty
 * if it happened here. In the common case, where the publisher does not need to signal, it would be
 * good to avoid using locks altogether. However, any alternative solutions need to make the
 * following guarantees:
 *  - If a consumer decides to wait, the publisher must signal when it releases the slot that the
 *    consumer was waiting for
 *  - The publisher must signal _after_ the consumer has fallen asleep, or the consumer will not be
 *    woken up
 *
 * # Approach
 *
 * We need a way for the publisher to synchronize with potential waiters at the point where it
 * checks if it needs to signal or not. If it finds that it does not need to signal, we need to be
 * able to prove that the consumer will not sleep. If it does see a need to signal, then it must be
 * assured that it will do so after the consumer has fallen asleep.
 *
 * # Algorithm
 *
 * The publisher executes the following steps whenever releasing items:
 *  - Release items via an atomic operation with release semantics (before calling notifyAllWaiters)
 *  - Check if there are any waiters using a read-modify-write operation on a shared variable (with
 *    rel-acq semantics)
 *  - If so, acquire the lock and signal on the wait condition
 *
 * The consumer executes the following steps before going to sleep:
 *  - Acquire the lock
 *  - Express an intent to sleep using a read-modify-write operation (with rel-acq semantics) on the
 *    shared variable
 *  - Check for any newly released items
 *  - If none, go to sleep, otherwise release the lock and finish waiting
 *
 * # Proof of correctness
 *
 * Two things need to be proven to show correctness:
 *  - the publisher will signal when necessary
 *  - the signal happens only after the waiter(s) have gone to sleep.
 *
 * Due to the use of read-modify-write operations, the race between publisher and consumer to access
 * the shared variable has two simple outcomes: either the publisher checks first and refrains from
 * signalling, or the consumer signals intent to sleep first, which the publisher will then see and
 * act upon.
 *
 * If the publisher accesses the shared variable before consumer has signalled intent to sleep: as
 * long as the item has been released before the access, we can say that:
 *  - the item release happens before the publisher's shared variable access
 *  - which happens before the consumer's shared variable access
 *  - which happens before the consumer's final check for a newly released item before sleeping
 * Therefore, if the publisher concludes that it doesn't need to signal, it is certain that the
 * consumer will see the newly released item and refrain from sleeping. In other words, a consumer
 * cannot go to sleep without the publisher seeing its intent to do so.
 *
 * An alternate proof based on the consumer deciding to sleep: due to acquire semantics on the
 * shared variable access, we know that the consumer accesses the shared variable before checking
 * for available items.  If the consumer, after taking the lock but before going to sleep, doesn't
 * see a newly released item, the following happens-before ordering is implied: consumer shared
 * variable access -> consumer check for new item -> publisher release of new item -> publisher
 * access of shared variable. Therefore, if the waiter decides to sleep after seeing no progress
 * from the publisher, we can say for sure that the publisher will see the signalled intent to
 * sleep, acquire the lock, and signal on the wait condition.
 *
 * Regarding the second issue of whether the waiter goes to sleep before the signal wakes it up,
 * observe that the consumer only expresses intent to sleep (through the shared variable) after
 * acquiring the lock. Thus, the producer cannot see the signal until after the consumer acquires
 * the lock, and cannot acquire the lock and signal until after the consumer has atomically released
 * the lock and slept.
 *
 * Unfortunately, this proof depends on having release semantics on the publisher side, and acquire
 * semantics on the waiting side. Release semantics require a store operation, while Acquire
 * semantics require a load operation. Therefore, the publisher needs to load the shared variable to
 * see if it needs to signal or not, while simultaneously storing to it in order to gain release
 * semantics.  Likewise, in addition to modifying the shared variable to signal intent, the waiters
 * need to also perform a load in order to have acquire semantics. This is why read-modify-write
 * operations are needed, and not just the cheaper load/store operations.
 */
pub struct BlockingWaitStrategy {
    d: UncheckedUnsafeArc<BlockingWaitStrategyData>,
    // Deep copyable fields that don't need to be shared
    /// Number of times to wait before blocking
    max_spin_tries_publisher: uint,
    /// Number of times to wait for a consumer sequence before yielding
    max_spin_tries_consumer: uint,
}

struct BlockingWaitStrategyData {
    /// True if any tasks are waiting for new slots to be released by the publisher.
    signal_needed: AtomicBool,
    /// Waiting consumers block on, and are signalled by, this.
    wait_condition: Mutex,
}

impl BlockingWaitStrategy {
    pub fn new() -> BlockingWaitStrategy {
        BlockingWaitStrategy::new_with_retry_count(
            default_max_spin_tries_publisher,
            default_max_spin_tries_consumer
        )
    }

    /**
     * Create a BlockingWaitStrategy, explicitly specifying how many times to spin before
     * transitioning to a yielding strategy.
     *
     * # Arguments
     *
     * See YieldWaitStrategy::new_with_retry_count for a more detailed description of what the
     * arguments mean. This wait strategy will block instead of yielding when the maximum number of
     * retries is reached while waiting for the publisher.
     */
    pub fn new_with_retry_count(
        max_spin_tries_publisher: uint,
        max_spin_tries_consumer: uint
    ) -> BlockingWaitStrategy {
        let d = BlockingWaitStrategyData {
            signal_needed: AtomicBool::new(false),
            wait_condition: Mutex::new_with_condvars(1),
        };
        BlockingWaitStrategy {
            d: UncheckedUnsafeArc::new(d),
            max_spin_tries_publisher: max_spin_tries_publisher,
            max_spin_tries_consumer: max_spin_tries_consumer,
        }
    }
}

impl Clone for BlockingWaitStrategy {
    /**
     * Returns a shallow copy, that waits and signals on the same wait condition.
     */
    fn clone(&self) -> BlockingWaitStrategy {
        BlockingWaitStrategy {
            d: self.d.clone(),
            max_spin_tries_publisher: self.max_spin_tries_publisher,
            max_spin_tries_consumer: self.max_spin_tries_consumer,
        }
    }
}

impl ProcessingWaitStrategy for BlockingWaitStrategy {
    fn wait_for_publisher(
        &mut self,
        n: uint,
        waiting_sequence: SequenceNumber,
        cursor: &SequenceReader,
        buffer_size: uint
    ) -> uint {
        let mut available = spin_for_publisher_retries(n, waiting_sequence, cursor, buffer_size,
            self.max_spin_tries_publisher);

        if available >= n {
            return available;
        }

        // Transition to blocking on wait condition
        let d;
        unsafe {
            d = self.d.get();
        }

        // Grab lock on wait condition
        do d.wait_condition.lock_cond |cond| {
            while n > available {
                // Communicate intent to wait to publisher
                let _dummy: bool = d.signal_needed.swap(true, AcqRel);
                // Verify that no slot was published
                available = calculate_available_consumer(cursor.get(), waiting_sequence, buffer_size);
                if n > available {
                    // Sleep
                    cond.wait();
                    available = calculate_available_consumer(cursor.get(), waiting_sequence, buffer_size);
                }
            }
        }

        available
    }
}

impl PublishingWaitStrategy for BlockingWaitStrategy {
    fn wait_for_consumers(
        &self,
        n: uint,
        waiting_sequence: SequenceNumber,
        dependencies: &[SequenceReader],
        buffer_size: uint,
        calculate_available: &fn(
            gating_sequence: SequenceNumber,
            waiting_sequence: SequenceNumber,
            batch_size: uint
        ) -> uint
    ) -> uint {
        let w = YieldWaitStrategy::new_with_retry_count(
            self.max_spin_tries_publisher, self.max_spin_tries_consumer
        );

        w.wait_for_consumers(n, waiting_sequence, dependencies, buffer_size, calculate_available)
    }

    fn notifyAllWaiters(&mut self) {
        let d;
        unsafe {
            d = self.d.get();
        }

        // Check if there are any waiters, resetting the value to false
        let signal_needed = d.signal_needed.swap(false, AcqRel);

        // If so, acquire the lock and signal on the wait condition
        if signal_needed {
            do d.wait_condition.lock_cond |cond| {
                cond.broadcast();
            }

            // This is a bit of a hack to work around the fact that the Mutex will occasionally
            // start executing the publisher's task when the consumer unlocks it, starving the
            // consumer. Doing this should cause the consumer to execute again, avoiding deadlock.
            // At the same time, it's mostly off the fast path (this code path is only hit if a long
            // gap in publishing caused one or more consumers to sleep), so performance shouldn't be
            // hurt much.
            task::deschedule();
        }
    }
}

impl fmt::Default for BlockingWaitStrategy {
    fn fmt(obj: &BlockingWaitStrategy, f: &mut fmt::Formatter) {
        write!(f.buf,
            "disruptor::BlockingWaitStrategy\\{p: {}, c: {}\\}",
            obj.max_spin_tries_publisher,
            obj.max_spin_tries_consumer
        );
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

    // Facilitate the default implementations of next_n and release_n
    /// Cache the passed in number of available slots for later use in get_cached_available.
    fn set_cached_available(&mut self, available: uint);
    /// Return the number of known-available slots as of the last read from the sequence barrier's
    /// gating sequence.
    fn get_cached_available(&self) -> uint;

    /**
     * Wait for a single slot to be available.
     */
    fn next<RB>(&mut self, rb: &mut RB) {
        self.next_n(1, rb)
    }

    /**
     * Wait for N slots to be available.
     *
     * # Arguments
     *
     * * batch_size - How many slots should be available before returning.
     * * rb - A reference to the underlying ring buffer, currently used to implement resizing.
     *
     * # Safety notes
     *
     * Note that if N is greater than the size of the RingBuffer minus the total number of slots the
     * rest of the pipeline is waiting for, then this function may deadlock. A size of 1 should
     * always be safe. Alternatively, increase the size of the buffer to support the desired amount
     * of batching.
     */
    fn next_n<RB>(&mut self, batch_size: uint, rb: &mut RB) {
        // Avoid waiting if the necessary slots were already available as of the last read. Calls
        // next_n_real if the slots are not available.
        if (self.get_cached_available() < batch_size) {
            let cached_available = self.next_n_real(batch_size, rb);
            self.set_cached_available(cached_available);
        }
    }

    /**
     * Wait for `batch_size` slots to become available, then return the actual number of available
     * slots, which may be greater than `batch_size`. This is called only as a last resort. If extra
     * slots were available as of the last wait, this function will not be called.
     */
    fn next_n_real<RB>(&mut self, batch_size: uint, rb: &mut RB) -> uint;

    /**
     * Release a single slot for downstream consumers.
     */
    fn release(&mut self) {
        self.release_n(1);
    }

    /**
     * Release n slots for downstream consumers.
     */
    fn release_n(&mut self, batch_size: uint) {
        // Update the cached value to reflect the newly used up slots, then call release_n_real.

        // Subtract batch_size from the cached number of available slots.
        let available = self.get_cached_available();
        assert!(available >= batch_size);
        self.set_cached_available(available - batch_size);

        self.release_n_real(batch_size);
    }

    /// Same as `release_n`,but without caching. This is called every time release_n is called.
    fn release_n_real(&mut self, batch_size: uint);
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
    /**
     * Contains the number of available items as of the last time the dependent sequence values were
     * retrieved.
     */
    cached_available: uint,
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
            cached_available: 0,
        }
    }

    /**
     * Assign a new set of dependencies to this barrier.
     *
     * # TODO
     *
     * After settling on a design for concurrent producers and perhaps concurrent consumers,
     * redesign dependencies to be immutable after the object is constructed.
     */
    fn set_dependencies(&mut self, dependencies: ~[SequenceReader]) {
        self.dependencies = dependencies;
    }
}

impl<W: PublishingWaitStrategy> SequenceBarrier for SinglePublisherSequenceBarrier<W> {
    fn get_current(&self) -> SequenceNumber { self.sequence.get_owned() }
    fn set_cached_available(&mut self, available: uint) { self.cached_available = available }
    fn get_cached_available(&self) -> uint { self.cached_available }

    fn next_n_real<RB>(&mut self, batch_size: uint, _rb: &mut RB) -> uint {
        let current_sequence = self.sequence.get_owned();
        let available = self.wait_strategy.wait_for_consumers(batch_size, current_sequence, self.dependencies, self.buffer_size, calculate_available_publisher);
        available
    }

    fn release_n_real(&mut self, batch_size: uint) {
        self.sequence.advance_and_flush(batch_size, self.buffer_size);
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
        }
    }
}

impl<W: ProcessingWaitStrategy> SequenceBarrier for SingleConsumerSequenceBarrier<W> {
    fn get_current(&self) -> SequenceNumber {
        self.sb.get_current()
    }
    fn set_cached_available(&mut self, available: uint) { self.sb.set_cached_available(available) }
    fn get_cached_available(&self) -> uint { self.sb.get_cached_available() }

    fn next_n_real<RB>(&mut self, batch_size: uint, _rb: &mut RB) -> uint {
        let current_sequence = self.get_current();
        let available = self.sb.wait_strategy.wait_for_publisher(batch_size, current_sequence, &self.cursor, self.sb.buffer_size);
        let a = self.sb.wait_strategy.wait_for_consumers(batch_size, current_sequence, self.sb.dependencies, self.sb.buffer_size, calculate_available_consumer);
        // wait_for_consumers returns uint::max_value if there are no other dependencies
        cmp::min(available, a)
    }

    fn release_n_real(&mut self, batch_size: uint) {
        self.sb.sequence.advance(batch_size, self.sb.buffer_size);
        // If the next call to next_n will result in more waiting, then make our progress visible to
        // downstream consumers now.
        if (self.sb.cached_available < batch_size) {
            self.sb.sequence.flush();
        }
    }
}

/**
 * Allows callers to wire up dependencies, then send values down the pipeline
 * of dependent consumers.
 */
struct SinglePublisher<T, W, RB> {
    rb: RB,
    sequence_barrier: SinglePublisherSequenceBarrier<W>,
}

impl<T: Send, W: ProcessingWaitStrategy> SinglePublisher<T, W, RingBuffer<T>> {
    /**
     * Constructs a new (non-resizeable) ring buffer with _size_ elements and wraps it into a new
     * SinglePublisher<T> object.
     */
    pub fn new(size: uint, wait_strategy: W) -> SinglePublisher<T, W, RingBuffer<T>> {
        SinglePublisher::new_common(size, wait_strategy)
    }
}

impl<T: Send, W: ProcessingWaitStrategy, RB: RingBufferTrait<T>> SinglePublisher<T, W, RB> {
    /// Generic constructor that works with any RingBufferTrait-conforming type
    fn new_common(size: uint, wait_strategy: W) -> SinglePublisher<T, W, RB> {
        SinglePublisher {
            rb: RingBufferTrait::<T>::new(size),
            sequence_barrier: SinglePublisherSequenceBarrier::new(~[], wait_strategy, size),
        }
    }

    /**
     * Creates and returns a single consumer, which will receive items sent through the publisher.
     */
    pub fn create_single_consumer_pipeline(&mut self) -> SingleFinalConsumer<T, W, RB> {
        let (_, c) = self.create_consumer_pipeline(1);
        c
    }

    /**
     * Creates and returns a new SingleConsumer instance that waits on the given list of gating
     * sequences. Private helper function.
     */
    fn make_consumer(&mut self, dependencies: ~[SequenceReader]) -> SingleConsumer<T, W, RB> {
        SingleConsumer::<T, W, RB>::new(
            self.rb.clone(),
            self.sequence_barrier.sequence.clone_immut(),
            dependencies,
            self.sequence_barrier.wait_strategy.clone()
        )
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
    pub fn create_consumer_pipeline(
        &mut self,
        count_consumers: uint
    ) -> (~[SingleConsumer<T, W, RB>], SingleFinalConsumer<T, W, RB>) {
        assert!(self.sequence_barrier.dependencies.len() == 0, "The create_consumer_pipeline method can only be called once.");

        // Create each stage in the chain, adding the previous stage as a gating dependency

        // The first stage consumers wait on the publisher's sequence, which is provided to all
        // consumers regardless of their position in the pipeline. It's not necessary to provide a
        // second reference to the same sequence, so an empty array is provided instead.
        let mut dependencies = ~[];

        let count_nonfinal_consumers = count_consumers - 1;
        let mut nonfinal_consumers = vec::with_capacity::<SingleConsumer<T, W, RB>>(count_nonfinal_consumers);
        let final_consumer;
        for _ in range(0, count_nonfinal_consumers) {
            let c = self.make_consumer(dependencies);
            dependencies = ~[c.get_sequence()];

            nonfinal_consumers.push(c);
        }

        // Last consumer gets the ability to take ownership
        let c = self.make_consumer(dependencies);
        dependencies = ~[c.get_sequence()];
        final_consumer = SingleFinalConsumer::new(c);

        self.sequence_barrier.set_dependencies(dependencies);

        (nonfinal_consumers, final_consumer)
    }

    pub fn publish(&self, value: T) {
        unsafe {
            // FIXME #5372
            let self_mut = cast::transmute_mut(self);
            let begin: SequenceNumber = self.sequence_barrier.get_current();
            // Wait for available slot
            self_mut.sequence_barrier.next(&mut self_mut.rb);
                self_mut.rb.set(begin, value);
            // Make the item available to downstream consumers
            self_mut.sequence_barrier.release();
        }
    }
}

/**
 * Allows callers to retrieve values from upstream tasks in the pipeline.
 */
struct SingleConsumer<T, W, RB> {
    rb: RB,
    sequence_barrier: SingleConsumerSequenceBarrier<W>,
}

impl<T: Send, W: ProcessingWaitStrategy, RB: RingBufferTrait<T>> SingleConsumer<T, W, RB> {
    fn new(
        rb: RB,
        cursor: SequenceReader,
        dependencies: ~[SequenceReader],
        wait_strategy: W
    ) -> SingleConsumer<T, W, RB> {
        let size = rb.size();
        SingleConsumer {
            rb: rb,
            sequence_barrier: SingleConsumerSequenceBarrier::new(cursor, dependencies, wait_strategy, size)
        }
    }

    /**
     * Returns a read-only reference to this consumer's sequence number.
     */
    fn get_sequence(&self) -> SequenceReader {
        self.sequence_barrier.sb.sequence.clone_immut()
    }

    /**
     * Waits for a single item to become available, then calls the given function to process the
     * value.
      */
    pub fn consume(&self, consume_callback: &fn(value: &T)) {
        unsafe {
            // FIXME #5372
            let self_mut = cast::transmute_mut(self);
            self_mut.sequence_barrier.next(&mut self_mut.rb);
            consume_callback(self_mut.rb.get(self.sequence_barrier.get_current()));
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
        let consumer = publisher.create_single_consumer_pipeline();
        publisher.publish(1);
        consumer.consume(|value: &int| {
            assert!(*value == 1);
        });
    }
    #[test]
    fn send_single_value_via_take() {
        let mut publisher = SinglePublisher::<int, SpinWaitStrategy>::new(1, SpinWaitStrategy);
        let consumer = publisher.create_single_consumer_pipeline();
        let value = 1;
        publisher.publish(value);
        let received_value = consumer.take();
        assert_eq!(received_value, value);
    }

    // TODO: test that dependencies hold true by setting up a chain, grabbing a list of timestamps
    // within each task, and then verifying them after for a happens-before relationship. It's not
    // foolproof, but better than nothing.
}

/**
 * The last consumer in a disruptor pipeline. Being the last consumer in the pipeline makes it
 * possible to move values out of the ring buffer, in addition to the functionality available from a
 * normal SingleConsumer.
 */
struct SingleFinalConsumer<T, W, RB> {
    sc: SingleConsumer<T, W, RB>
}

impl <T: Send, W: ProcessingWaitStrategy, RB: RingBufferTrait<T>> SingleFinalConsumer<T, W, RB> {
    /**
     * Return a new SingleFinalConsumer instance wrapped around a given SingleConsumer instance.
     * In addition to existing SingleConsumer features, this object also allows the caller to take
     * ownership of the items that it accesses.
     */
    fn new(
        sc: SingleConsumer<T, W, RB>
    ) -> SingleFinalConsumer<T, W, RB> {
        SingleFinalConsumer {
            sc: sc
        }
    }

    /// See the SingleConsumer.consume method.
    pub fn consume(&self, consume_callback: &fn(value: &T)) { self.sc.consume(consume_callback) }

    /**
     * Waits for the next value to be available, moves it out of the ring buffer, and returns it.
     */
    pub fn take(&self) -> T {
        unsafe {
            let sc = &mut cast::transmute_mut(self).sc;
            sc.sequence_barrier.next(&mut sc.rb);
            let value = sc.rb.take(sc.sequence_barrier.get_current());
            sc.sequence_barrier.release();
            value
        }
    }
}
