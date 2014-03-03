// Copyright 2013 Simon Ruggier.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.
extern crate sync;
extern crate time;
use self::sync::Mutex;
use self::time::precise_time_ns;
use std::clone::Clone;
use std::cast;
use std::cmp;
use std::fmt;
use std::num::Bitwise;
use std::option::{Option};
use std::ptr;
use std::task;
use std::vec;
use std::uint;
use std::sync::arc::UnsafeArc;
use std::sync::atomics::{AtomicUint,Acquire,Release,AtomicBool,AcqRel};

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
    fn new() -> Slot<T> {
        let payload: ~Option<T> = ~None;
        let payload_raw: *mut Option<T> = unsafe { cast::transmute(payload) };
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
     * Destroys the value, if present, and assigns a null value. This can be used with `is_set` as a
     * signalling mechanism.
     *
     * # Safety notes
     *
     * It's the caller's responsibility not to call this after destroy.
     */
    unsafe fn unset(&mut self) {
        (*self.payload) = None;
    }

    /**
     * Checks if a value is set in the slot.
     *
     * # Safety notes
     *
     * It's the caller's responsibility not to call this after destroy.
     */
    unsafe fn is_set(&self) -> bool {
        (*self.payload).is_some()
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
        let buffer = vec::from_fn(size, |_i| { Slot::new() } );
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
            assert!(self.entries[index].is_set(), "Take of None at sequence: {:?}", sequence.value());
            self.entries[index].take()
        }
    }

    /**
     * Assigns a special null value to the slot pointed to by `sequence`, which can be checked using
     * the `is_set` method.
     */
    fn unset(&mut self, sequence: SequenceNumber) {
        let index = sequence.as_index(self.size());
        unsafe {
            self.entries[index].unset();
        }
    }

    /**
     * Checks whether the slot corresponding to `sequence` contains a value or not.
     */
    fn is_set(&self, sequence: SequenceNumber) -> bool {
        let index = sequence.as_index(self.size());
        unsafe {
            self.entries[index].is_set()
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
        // assert!(buffer_size.count_ones() == 1, "buffer_size must be a power of two (received {:?})", buffer_size);
        let index_mask = buffer_size - 1;
        let SequenceNumber(value) = self;
        value & index_mask
    }

    /**
     * Return the SequenceNumber's uint value. For when a destructuring let isn't concise enough.
     */
    fn value(self) -> uint {
        let SequenceNumber(value) = self;
        value
    }
}

/**
 * Returns the number at which sequence values will be wrapped back to 0 using a mod operation.
 * The returned number will be a power of two, and a multiple of buffer_size.
 */
fn wrap_boundary(buffer_size: uint) -> uint {
    4*buffer_size
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
 * A ring buffer that takes `SequenceNumber` values for get and set operations. Buffer lifetime is
 * managed using reference counting.
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
trait RingBufferTrait<T> : RingBufferOps<T> + Clone { }
// Automatically apply RingBufferTrait to qualifying types
impl<T: Send, RB: RingBufferOps<T> + Clone> RingBufferTrait<T> for RB {}

/**
 * Allows the actual operations a ring buffer exposes to be wrapped by other traits without also
 * bringing in the Send + Clone bounds.
 */
trait RingBufferOps<T: Send> : Send {
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
    /**
     * Constructs a new RingBuffer with a capacity of `size` elements. The size must be a power of
     * two, a property that will be exploited for performance reasons.
     */
    fn new(size: uint) -> RingBuffer<T> {
        assert!(size.count_ones() == 1, "RingBuffer size must be a power of two (received {:?})", size);
        let data = RingBufferData::new(size);
        RingBuffer { data: UncheckedUnsafeArc::new(data) }
    }

    /// See `RingBufferData::unset`. Unsafe: allows data races.
    unsafe fn unset(&mut self, sequence: SequenceNumber) {
        let d = self.data.get();
        d.unset(sequence);
    }

    /// See `RingBufferData::is_set`. Unsafe: allows data races.
    unsafe fn is_set(&self, sequence: SequenceNumber) -> bool {
        let d = self.data.get_immut();
        d.is_set(sequence)
    }
}

impl<T: Send> RingBufferOps<T> for RingBuffer<T> {
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
    let SequenceNumber(mut gating) = gating_sequence;
    let SequenceNumber(waiting) = waiting_sequence;
    // Handle wrapping. Also, if the publisher has reallocated a larger buffer, it won't wrap until
    // all consumers have reached the largest buffer, so we can be sure that this code path won't be
    // hit while the consumer is working with the old buffer size.
    if gating < waiting {
        gating += wrap_boundary(buffer_size);
    }
    let available = gating - waiting;
    // No longer a valid assumption, given the possibility of resizable buffers
    // assert!(available <= buffer_size, "available: {:?}, gating: {:?}, waiting: {:?}", available, gating, waiting);
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

    // Test wrapping (publisher wraps to 0 at wrap_boundary(buffer_size) )
    assert!(7 == calculate_available_consumer(SequenceNumber(31), SequenceNumber(24), 8));
    assert!(8 == calculate_available_consumer(SequenceNumber(0), SequenceNumber(24), 8));
    assert!(7 == calculate_available_consumer(SequenceNumber(0), SequenceNumber(25), 8));

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
    let SequenceNumber(gating_value) = gating_sequence;
    let SequenceNumber(waiting_value) = waiting_sequence;
    let mut available = gating_value + buffer_size - waiting_value;
    // Handle wrapping
    if available > buffer_size {
        // In this case, we know that the value of available is exactly wrap_boundary(buffer_size)
        // more than it should be. Mask out the extra slots, taking advantage of the fact that
        // buffer_size and wrap boundary are powers of 2.
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
     * around when it reaches wrap_boundary(buffer_size). This results in two easily distinguishable
     * states for the availability calculations to handle. Consumer sequences are normally behind
     * gating sequences, whether they are owned by other consumers or the publisher. However, the
     * gating sequence will wrap first, and remain behind until consumers reach the wrapping
     * boundary, at which point they will also wrap. The publisher is normally ahead of the sequence
     * it depends on, but after wrapping, it will be temporarily behind the gating sequence.
     */
    fn advance(&mut self, n: uint, buffer_size: uint) {
        // NOTE: Mutating the private value here, and in the unwrap function, is safe because this
        // type's API doesn't allow it to be cloned and accessed from multiple places concurrently.
        unsafe {
            let d = self.value_arc.get();
            d.private_value += n;
            // Given that buffer_size is a power of two, wrap by masking out the high bits. This
            // operation is a noop if the value is less than wrap_boundary(buffer_size), so it's
            // unnecessary to check before wrapping.
            let wrap_mask = wrap_boundary(buffer_size) - 1;
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

    /**
     * Reverses the effects of wrapping that occur in the advance function.
     */
    fn unwrap(&mut self, buffer_size: uint) {
        unsafe {
            let d = self.value_arc.get();
            let SequenceNumber(unwrapped) = Sequence::unwrap_number(SequenceNumber(d.private_value), buffer_size);
            d.private_value = unwrapped;
        }
    }

    /**
     * Like unwrap, but for standalone SequenceNumber values.
     */
    fn unwrap_number(sn: SequenceNumber, buffer_size: uint) -> SequenceNumber {
        let SequenceNumber(value) = sn;
        assert!(value < wrap_boundary(buffer_size));
        // We know the sequence value is in the interval [0, 4*buffer_size). This expression
        // ensures that it will be within [4*buffer_size, 5*buffer_size) instead.
        let buffer_size_mask = buffer_size - 1;
        let new_value = (value & buffer_size_mask) + wrap_boundary(buffer_size);
        SequenceNumber(new_value)
    }
}

fn log2(mut power_of_2: uint) -> uint {
    assert!(power_of_2.count_ones() == 1, "Argument must be a power of two (received {:?})", power_of_2);
    let mut exp = 0;
    while power_of_2 > 1 {
        exp += 1;
        power_of_2 >>= 1;
    }
    exp
}

/// Ensure sequences correctly handle buffer sizes of 2^(uint::BITS-1).
#[test]
fn test_sequence_overflow() {
    // The maximum buffer size is 2^(uint::BITS) / wrap_boundary(1) (for example, 2^30 with the
    // current boundary of 4*buffer_size). For that size, wrap_boundary(buffer_size) - 1 would
    // evaluate to uint::MAX, and unsigned integer arithmetic will naturally take care of the
    // wrapping. The sequence will wrap to 0 at wrap_boundary(buffer_size), i.e. uint::MAX + 1.
    let exp = log2(wrap_boundary(1));
    let max_buffer_size = 1 << (uint::BITS - exp);

    let mut s = Sequence::new();
    assert_eq!(s.get().value(), SEQUENCE_INITIAL);

    // Add 1
    s.advance_and_flush(1, max_buffer_size);
    let incremented_value = s.get().value();
    assert_eq!(incremented_value, SEQUENCE_INITIAL + 1);

    // Advance to max_buffer_size
    s.advance_and_flush(max_buffer_size - incremented_value, max_buffer_size);
    assert_eq!(s.get().value(), max_buffer_size);

    // Overflow to 4*max_buffer_size + 1 and confirm that it wrapped to 1
    s.advance_and_flush(3*max_buffer_size + 1, max_buffer_size);
    assert_eq!(s.get().value(), 1);
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
    assert!(0 == reader.get().value());
    sequence.advance_and_flush(1, buffer_size);
    assert!(1 == reader.get().value());
    sequence.advance_and_flush(11, buffer_size);
    assert!(12 == reader.get().value());
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
     * uint::MAX if there are no dependencies.
     */
    fn wait_for_consumers(
        &self,
        n: uint,
        waiting_sequence: SequenceNumber,
        dependencies: &[SequenceReader],
        buffer_size: uint,
        calculate_available: |gating_sequence: SequenceNumber, waiting_sequence: SequenceNumber, batch_size: uint| -> uint
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
    calculate_available: &|gating_sequence: SequenceNumber, waiting_sequence: SequenceNumber, batch_size: uint| -> uint
) -> uint {
    let mut available = uint::MAX;
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
        calculate_available: |gating_sequence: SequenceNumber, waiting_sequence: SequenceNumber, batch_size: uint| -> uint
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

impl fmt::Show for SpinWaitStrategy {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f.buf, "disruptor::SpinWaitStrategy")
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
    calculate_available: & |gating_sequence: SequenceNumber, waiting_sequence: SequenceNumber, batch_size: uint| -> uint,
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
pub struct YieldWaitStrategy {
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
        calculate_available: |
            gating_sequence: SequenceNumber,
            waiting_sequence: SequenceNumber,
            batch_size: uint
        | -> uint
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

impl fmt::Show for YieldWaitStrategy {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f.buf,
            "disruptor::YieldWaitStrategy\\{p: {}, c: {}\\}",
            self.max_spin_tries_publisher,
            self.max_spin_tries_consumer
        )
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
        let signal_needed = &mut d.signal_needed;
        d.wait_condition.lock_cond(|cond| {
            while n > available {
                // Communicate intent to wait to publisher
                let _dummy: bool = signal_needed.swap(true, AcqRel);
                // Verify that no slot was published
                available = calculate_available_consumer(cursor.get(), waiting_sequence, buffer_size);
                if n > available {
                    // Sleep
                    cond.wait();
                    available = calculate_available_consumer(cursor.get(), waiting_sequence, buffer_size);
                }
            }
        });

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
        calculate_available: |
            gating_sequence: SequenceNumber,
            waiting_sequence: SequenceNumber,
            batch_size: uint
        | -> uint
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
            d.wait_condition.lock_cond(|cond| {
                cond.broadcast();
            });

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

impl fmt::Show for BlockingWaitStrategy {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f.buf,
            "disruptor::BlockingWaitStrategy\\{p: {}, c: {}\\}",
            self.max_spin_tries_publisher,
            self.max_spin_tries_consumer
        )
    }
}

/**
 * Responsible for ensuring that the caller does not proceed until one or more dependent sequences
 * have finished working with the subsequent slots.
 */
pub trait SequenceBarrier<T> : Send {
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
    fn next(&mut self) {
        self.next_n(1)
    }

    /**
     * Wait for N slots to be available.
     *
     * # Arguments
     *
     * * batch_size - How many slots should be available before returning.
     *
     * # Safety notes
     *
     * Note that if N is greater than the size of the RingBuffer minus the total number of slots the
     * rest of the pipeline is waiting for, then this function may deadlock. A size of 1 should
     * always be safe. Alternatively, increase the size of the buffer to support the desired amount
     * of batching.
     */
    fn next_n(&mut self, batch_size: uint) {
        // Avoid waiting if the necessary slots were already available as of the last read. Calls
        // next_n_real if the slots are not available.
        if (self.get_cached_available() < batch_size) {
            let cached_available = self.next_n_real(batch_size);
            self.set_cached_available(cached_available);
        }
    }

    /**
     * Wait for `batch_size` slots to become available, then return the actual number of available
     * slots, which may be greater than `batch_size`. This is called only as a last resort. If extra
     * slots were available as of the last wait, this function will not be called.
     */
    fn next_n_real(&mut self, batch_size: uint) -> uint;

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

    /**
     * Returns a read-only reference to this barrier's sequence number.
     */
    fn get_sequence(&self) -> SequenceReader;

    /// Returns a borrowed pointer to the dependency list.
    fn get_dependencies<'s>(&'s self) -> &'s [SequenceReader];

    /**
     * Assign a new set of dependencies to this barrier.
     */
    fn set_dependencies(&mut self, dependencies: ~[SequenceReader]);

    // Ring buffer related operations

    /// Returns the size of the underlying ring buffer.
    fn size(&self) -> uint;

    /**
     * Stores a value in the sequence barrier's current slot.
     *
     * # Safety notes
     *
     * It's the caller's responsibility to avoid data races, so this function is unsafe. Races could
     * occur in cases where multiple barriers are waiting on the same dependency and accessing slots
     * in parallel.
     */
    unsafe fn set(&mut self, value: T);

    /**
     * Gets the value stored in the sequence barrier's current slot, which would have been stored
     * there by a different task, in most cases). Unsafe: allows data races.
     *
     * Mutable to facilitate transparent transitions to larger buffers.
     */
    unsafe fn get<'s>(&'s mut self) -> &'s T;

    /**
     * Takes the value stored in the sequnce barrier's current slot, moving it out of the ring
     * buffer. Unsafe: allows data races.
     */
    unsafe fn take(&mut self) -> T;
}

/**
 * Split off from SequenceBarrier to reduce unnecessary type parameter requirements for general
 * users of the SequenceBarrier type, and users of the Publisher type.
 */
trait NewConsumerBarrier<CSB> {
    /**
     * Constructs a consumer barrier that is set up to wait on this SequenceBarrier's sequence
     * before it attempts to process items.
     */
    fn new_consumer_barrier(&self) -> CSB;
}

/**
 * Implements `SequenceBarrier` for publishers in situations where there's only one concurrent
 * publisher.
 */
struct SinglePublisherSequenceBarrier<W, RB> {
    ring_buffer: RB,
    sequence: Sequence,
    dependencies: ~[SequenceReader],
    wait_strategy: W,
    /**
     * Contains the number of available items as of the last time the dependent sequence values were
     * retrieved.
     */
    cached_available: uint,
}

impl<T: Send, W: PublishingWaitStrategy, RB: RingBufferTrait<T>> SinglePublisherSequenceBarrier<W, RB> {
    fn new(
        ring_buffer: RB,
        dependencies: ~[SequenceReader],
        wait_strategy: W
    ) -> SinglePublisherSequenceBarrier<W, RB> {
        SinglePublisherSequenceBarrier {
            ring_buffer: ring_buffer,
            sequence: Sequence::new(),
            dependencies: dependencies,
            wait_strategy: wait_strategy,
            cached_available: 0
        }
    }
}

impl<T: Send, W: ProcessingWaitStrategy, RB: RingBufferTrait<T>> SequenceBarrier<T>
        for SinglePublisherSequenceBarrier<W, RB> {
    fn get_current(&self) -> SequenceNumber { self.sequence.get_owned() }
    fn set_cached_available(&mut self, available: uint) { self.cached_available = available }
    fn get_cached_available(&self) -> uint { self.cached_available }
    fn get_dependencies<'s>(&'s self) -> &'s [SequenceReader] {
        // Work around type inference problem
        let d: &'s [SequenceReader] = self.dependencies;
        d
    }
    fn set_dependencies(&mut self, dependencies: ~[SequenceReader]) {
        self.dependencies = dependencies;
    }
    fn get_sequence(&self) -> SequenceReader { self.sequence.clone_immut() }

    fn next_n_real(&mut self, batch_size: uint) -> uint {
        let current_sequence = self.sequence.get_owned();
        let available = self.wait_strategy.wait_for_consumers(batch_size, current_sequence,
                self.dependencies, self.ring_buffer.size(), calculate_available_publisher);
        available
    }

    fn release_n_real(&mut self, batch_size: uint) {
        self.sequence.advance_and_flush(batch_size, self.ring_buffer.size());
        self.wait_strategy.notifyAllWaiters();
    }

    fn size(&self) -> uint { self.ring_buffer.size() }

    unsafe fn set(&mut self, value: T) {
        let current_sequence = self.get_current();
        self.ring_buffer.set(current_sequence, value)
    }

    unsafe fn get<'s>(&'s mut self) -> &'s T {
        let current_sequence = self.get_current();
        self.ring_buffer.get(current_sequence)
    }

    unsafe fn take(&mut self) -> T {
        let current_sequence = self.get_current();
        self.ring_buffer.take(current_sequence)
    }
}

impl<T: Send, W: ProcessingWaitStrategy, RB: RingBufferTrait<T>>
        NewConsumerBarrier<SingleConsumerSequenceBarrier<W, RB>>
        for SinglePublisherSequenceBarrier<W, RB> {
    fn new_consumer_barrier(&self) -> SingleConsumerSequenceBarrier<W, RB> {
        SingleConsumerSequenceBarrier::new(
            self.ring_buffer.clone(),
            // Our sequence is the publisher's sequence (aka the cursor)
            self.sequence.clone_immut(),
            // The first stage consumers wait on the publisher's sequence, which is provided to all
            // consumers regardless of their position in the pipeline. It's not necessary to provide a
            // second reference to the same sequence, so an empty array is provided instead.
            ~[],
            self.wait_strategy.clone()
        )
    }
}

/**
 * Implements `SequenceBarrier` for consumers. This implementation supports multiple concurrent
 * consumers, but all consumers will process all events. This is unsuitable for when a
 * load-balancing arrangement is desired.
 */
struct SingleConsumerSequenceBarrier<W, RB> {
    sb: SinglePublisherSequenceBarrier<W, RB>,
    /**
     * A reference to the publisher's sequence.
     */
    cursor: SequenceReader,
}

impl<T: Send, W: ProcessingWaitStrategy, RB: RingBufferTrait<T>> SingleConsumerSequenceBarrier<W, RB> {
    fn new(
        ring_buffer: RB,
        cursor: SequenceReader,
        dependencies: ~[SequenceReader],
        wait_strategy: W
    ) -> SingleConsumerSequenceBarrier<W, RB> {
        SingleConsumerSequenceBarrier {
            sb: SinglePublisherSequenceBarrier::new(
                ring_buffer,
                dependencies,
                wait_strategy
            ),
            cursor: cursor,
        }
    }
}

impl<T: Send, W: ProcessingWaitStrategy, RB: RingBufferTrait<T>> SequenceBarrier<T>
        for SingleConsumerSequenceBarrier<W, RB> {
    fn get_current(&self) -> SequenceNumber { self.sb.get_current() }
    fn set_cached_available(&mut self, available: uint) { self.sb.set_cached_available(available) }
    fn get_cached_available(&self) -> uint { self.sb.get_cached_available() }
    fn get_dependencies<'s>(&'s self) -> &'s [SequenceReader] { self.sb.get_dependencies() }
    fn set_dependencies(&mut self, dependencies: ~[SequenceReader]) { self.sb.set_dependencies(dependencies) }
    fn get_sequence(&self) -> SequenceReader { self.sb.get_sequence() }

    fn next_n_real(&mut self, batch_size: uint) -> uint {
        let current_sequence = self.get_current();
        let available = self.sb.wait_strategy.wait_for_publisher(batch_size, current_sequence,
                &self.cursor, self.sb.ring_buffer.size());
        let a = self.sb.wait_strategy.wait_for_consumers(batch_size, current_sequence,
                self.sb.dependencies, self.sb.ring_buffer.size(), calculate_available_consumer);
        // wait_for_consumers returns uint::MAX if there are no other dependencies
        cmp::min(available, a)
    }

    fn release_n_real(&mut self, batch_size: uint) {
        self.sb.sequence.advance(batch_size, self.sb.ring_buffer.size());
        // If the next call to next_n will result in more waiting, then make our progress visible to
        // downstream consumers now.
        if (self.sb.cached_available < batch_size) {
            self.sb.sequence.flush();
        }
    }

    fn size(&self) -> uint { self.sb.size() }
    unsafe fn set(&mut self, value: T) { self.sb.set(value) }
    unsafe fn get<'s>(&'s mut self, ) -> &'s T { self.sb.get() }
    unsafe fn take(&mut self) -> T { self.sb.take() }
}

impl<T: Send, W: ProcessingWaitStrategy, RB: RingBufferTrait<T>>
        NewConsumerBarrier<SingleConsumerSequenceBarrier<W, RB>>
        for SingleConsumerSequenceBarrier<W, RB> {
    fn new_consumer_barrier(&self) -> SingleConsumerSequenceBarrier<W, RB> {
        SingleConsumerSequenceBarrier::new(
            self.sb.ring_buffer.clone(),
            self.cursor.clone_immut(),
            ~[self.sb.sequence.clone_immut()],
            self.sb.wait_strategy.clone()
        )
    }
}

/**
 * Allows callers to wire up dependencies, then send values down the pipeline
 * of dependent consumers.
 */
pub struct Publisher<SB> {
    sequence_barrier: SB,
}

impl<T: Send, W: ProcessingWaitStrategy>
        Publisher<SinglePublisherSequenceBarrier<W, RingBuffer<T>>> {

    /**
     * Constructs a new (non-resizeable) ring buffer with _size_ elements and wraps it into a new
     * Publisher object.
     */
    pub fn new(size: uint, wait_strategy: W)
            -> Publisher<SinglePublisherSequenceBarrier<W, RingBuffer<T>>> {

        let ring_buffer =  RingBuffer::<T>::new(size);
        let sb = SinglePublisherSequenceBarrier::new(ring_buffer, ~[], wait_strategy);
        Publisher::<T, SinglePublisherSequenceBarrier<W, RingBuffer<T>> >::new_common(sb)
    }
}

impl<T: Send, SB: SequenceBarrier<T> > Publisher<SB> {
    /// Generic constructor that works with any RingBufferTrait-conforming type
    fn new_common(sb: SB) -> Publisher<SB> {
        Publisher {
            sequence_barrier: sb,
        }
    }

    pub fn publish(&self, value: T) {
        unsafe {
            // FIXME #5372
            let self_mut = cast::transmute_mut(self);
            // Wait for available slot
            self_mut.sequence_barrier.next();
            {
                self_mut.sequence_barrier.set(value);
            }
            // Make the item available to downstream consumers
            self_mut.sequence_barrier.release();
        }
    }
}

impl<
    T: Send,
    SB: SequenceBarrier<T> + NewConsumerBarrier<CSB>,
    CSB: SequenceBarrier<T> + NewConsumerBarrier<CSB>
> Publisher<SB> {
    /**
     * Creates and returns a single consumer, which will receive items sent through the publisher.
     */
    pub fn create_single_consumer_pipeline(&mut self) -> FinalConsumer<CSB> {
        let (_, c) = self.create_consumer_pipeline(1);
        c
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
    ) -> (~[Consumer<CSB>], FinalConsumer<CSB>) {
        assert!(self.sequence_barrier.get_dependencies().len() == 0, "The create_consumer_pipeline method can only be called once.");

        // Create each stage in the chain, adding the previous stage as a gating dependency

        let mut sb = self.sequence_barrier.new_consumer_barrier();

        let count_nonfinal_consumers = count_consumers - 1;
        let mut nonfinal_consumers =
                vec::with_capacity::<Consumer<CSB>>(count_nonfinal_consumers);
        let final_consumer;

        for _ in range(0, count_nonfinal_consumers) {
            let sb_next = sb.new_consumer_barrier();
            let c = Consumer::new(sb);
            sb = sb_next;

            nonfinal_consumers.push(c);
        }

        // Last consumer gets the ability to take ownership
        let dependencies = ~[sb.get_sequence()];
        let c = Consumer::new(sb);
        final_consumer = FinalConsumer::new(c);

        self.sequence_barrier.set_dependencies(dependencies);

        (nonfinal_consumers, final_consumer)
    }

}

/**
 * Allows callers to retrieve values from upstream tasks in the pipeline.
 */
struct Consumer<SB> {
    sequence_barrier: SB,
}

impl<T: Send, SB: SequenceBarrier<T>>
        Consumer<SB> {
    fn new(sb: SB) -> Consumer<SB> {
        Consumer {
            sequence_barrier: sb
        }
    }

    /**
     * Waits for a single item to become available, then calls the given function to process the
     * value.
      */
    pub fn consume(&self, consume_callback: |value: &T|) {
        unsafe {
            // FIXME #5372
            let self_mut = cast::transmute_mut(self);
            self_mut.sequence_barrier.next();
            {
                let item = self_mut.sequence_barrier.get();
                consume_callback(item);
            }
            self_mut.sequence_barrier.release();

        }
    }
}

#[cfg(test)]
mod single_publisher_tests {
    use super::{Publisher, SpinWaitStrategy};

    #[test]
    fn send_single_value() {
        let mut publisher = Publisher::<int, SpinWaitStrategy>::new(1, SpinWaitStrategy);
        let consumer = publisher.create_single_consumer_pipeline();
        publisher.publish(1);
        consumer.consume(|value: &int| {
            assert!(*value == 1);
        });
    }
    #[test]
    fn send_single_value_via_take() {
        let mut publisher = Publisher::<int, SpinWaitStrategy>::new(1, SpinWaitStrategy);
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
 * normal Consumer.
 */
pub struct FinalConsumer<SB> {
    sc: Consumer<SB>
}

impl <T: Send, SB: SequenceBarrier<T>>
        FinalConsumer<SB> {

    /**
     * Return a new FinalConsumer instance wrapped around a given Consumer instance. In
     * addition to existing Consumer features, this object also allows the caller to take ownership
     * of the items that it accesses.
     */
    fn new(sc: Consumer<SB>) -> FinalConsumer<SB> {
        FinalConsumer {
            sc: sc
        }
    }

    /// See the Consumer.consume method.
    pub fn consume(&self, consume_callback: |value: &T|) { self.sc.consume(consume_callback) }

    /**
     * Waits for the next value to be available, moves it out of the ring buffer, and returns it.
     */
    pub fn take(&self) -> T {
        unsafe {
            let sc = &mut cast::transmute_mut(self).sc;
            sc.sequence_barrier.next();
            let value;
            {
                 value = sc.sequence_barrier.take();
            }
            sc.sequence_barrier.release();
            value
        }
    }
}

/**
 * Now, implement a resizable version of the disruptor. After waiting sufficiently long enough for
 * the consumer pipeline to release slots, the publisher will instead allocate a new, larger, ring
 * buffer, write a special value to the corresponding slot in the old ring buffer, and store the
 * actual value in the new ring buffer. A pointer from the old buffer to the new one is also
 * written. The publisher then makes these changes visible to the downstream pipeline by
 * incrementing its sequence value. If the value has wrapped recently, the publisher bumps it back
 * above the consumers' sequence values, to avoid ambiguity resulting from the larger buffer size.
 * Finally, the last consumer in the pipeline deallocates the old buffer before moving on to the
 * larger buffer. Currently, the lifetime of old buffers is managed via reference counting.
 *
 * NOTE: Latency sensitive applications should not use this mode: the unbounded queue buildup will
 * increase latency outside of acceptable levels. Instead, they should gracefully handle excess
 * demand by providing whatever feedback is needed to reduce upstream demand to levels that the
 * application can handle. For example, this may involve dropping packets, queueing users that try
 * to open new sessions (to avoid degrading service for existing sessions), skipping frames, or any
 * other mechanism that reduces demand as early as possible to avoid wasted effort.
 *
 * # Availability calculation following reallocation
 *
 * Although it's not strictly necessary, things are most efficient if the publisher immediately uses
 * all of the slots in the newly allocated buffer without waiting for the consumer. After resizing
 * the buffer, the publisher will start using the increased buffer size in availability
 * calculations. This allows the publisher to use (new_size - old_size) extra slots, but it leaves
 * old_size slots unused.
 *
 * To fix this, the publisher manipulates its cached availability value to reflect the extra
 * available slots. This avoids the need to add extra code and state just to solve this problem.
 *
 * # Example
 *
 * Imagine a ring buffer of size 4. All stages in the pipeline start with a sequence of 0. The
 * publisher publishes 15 items, which leaves its sequence number at 15. For this to be possible,
 * the consumer(s) must have processed some of the items, since the buffer size, 4, is less than 15.
 * Actually, only 3 elements are available in the buffer, the 4th is reserved as a way for the
 * publisher to communicate that it has allocated another buffer. The publisher then publishes its
 * 16th item into the slot corresponding to sequence 15. When it increments its sequence number, it
 * wraps back down to 0, because the wrap boundary is 16 (four times the buffer size).
 *
 * Now, let's imagine the application is somehow written to contain a deadlock. For example, the
 * consumer is receiving from the publisher via two different communication channels, and it waits
 * for the publisher to send something on the second, while the publisher is waiting for the
 * consumer to process items from the first. When the resizing support is in use, the publisher will
 * eventually decide to reallocate a larger buffer of size 8.
 *
 * During the reallocation, the final consumer's sequence number is at least 13 (`16 - (buffer_size
 * - 1)`), since the publisher has been taking care to leave an extra slot free in case a
 * reallocation is needed. The consumer's sequence will never exceed the publisher's, except through
 * wrapping, so we can also say that it is logically at most 16, where 16 would be wrapped to 0.
 * Therefore, the consumer's sequence value could be any of {13, 14, 15, 0}.  The publisher's
 * sequence value is 0 until reallocation is complete, at which point it will be unwrapped back to
 * 16, and incremented to 17. One important thing to note is that regardless of whether the
 * publisher's sequence value was 0, 4, 8, or 12, it will be unwrapped to 16.
 *
 * If the consumer's sequence value was 13 prior to the reallocation, then there were 0 slots
 * available for the publisher to use: with a buffer size of 4, the last slot available to the
 * publisher would have been at sequence value 16, minus the one slot that was reserved for
 * signalling reallocations. In other words, 15 was the last available slot, and the publisher has
 * already used it.
 *
 * After the reallocation, though, the availability calculation would be using the larger buffer
 * size of 8, and as a result would conclude that the last available slot is 20 (subtracting one
 * leaves 19), so the publisher is now able to publish 4 more times into slots 16-19. However, there
 * are actually 7 more slots available (8 minus the one reserved slot). To use the extra slots, the
 * publisher modifies its cached availability value to indicate that 7 slots are available.  This
 * allows it to immediately publish into slots 20, 21, and 22 as well, leaving its sequence value at
 * 23 instead of 20.
 *
 * This results in several new special cases to handle versus the usual non-resizing variant:
 * - From the consumer's perspective, there may be more than `buffer_size` slots available, because
 * the publisher has started publishing into the new buffer.
 * - When calculating availability from the publisher's perspective, the consumer sequence that
 * gates its publishing may be more than `buffer_size` slots behind. The availability calculation
 * needs to return 0 in this case.
 * - It becomes important to ensure that the publisher does not wrap until the consumer pipeline has
 * transitioned to the new buffer, to avoid breaking the consumer's availability calculations.
 *
 * The publisher's availability calculation function was rewritten to correctly handle the second
 * point, and the wrap boundary was changed to `4*buffer_size` to facilitate the third point.
 */
struct ResizableRingBufferData<T> {
    rb_data: RingBufferData<T>,
    /**
     * When non-null, points to a larger buffer allocated by the publisher to replace this one.
     *
     * NOTE: The life cycle of old ring buffers is well defined (it should be deallocated either
     * when the last consumer(s) in the pipeline have finished retrieving items from it, or when the
     * publisher and all consumers have been destroyed). It is therefore possible to manage this
     * allocation without using reference counting. However, the reference count is only modified
     * when a reallocation occurs, so this suboptimal choice shouldn't have a strong effect on
     * performance. Implementing a more efficient solution is possible, but may not be worth the
     * extra code.
     */
    next: Option<UncheckedUnsafeArc<ResizableRingBufferData<T>>>,
}

impl<T: Send> ResizableRingBufferData<T> {
    /// Constructs a new ring buffer with the given size.
    fn new(size: uint) -> ResizableRingBufferData <T> {
        ResizableRingBufferData {
            rb_data: RingBufferData::new(size),
            next: None
        }
    }

    /**
     * Reallocates a larger ring buffer, stores a pointer to the new buffer in `next`, and marks the
     * corresponding slot in the old buffer to signal to consumers that there is a larger buffer.  A
     * reference to the new buffer is returned.
     */
    unsafe fn reallocate(&mut self, sequence: SequenceNumber, new_size: uint)
        -> UncheckedUnsafeArc<ResizableRingBufferData<T>> {

        let new_rrbd = ResizableRingBufferData::new(new_size);
        self.next = Some(UncheckedUnsafeArc::new(new_rrbd));
        self.rb_data.unset(sequence);
        self.next.get_mut_ref().clone()
    }

    // Functions "inherited" from RingBufferData
    /// See `RingBufferData::size`
    fn size(&self) -> uint { self.rb_data.size() }
    /// See `RingBufferData::set`
    fn set(&mut self, sequence: SequenceNumber, value: T) {
        self.rb_data.set(sequence, value);
    }
    /// See `RingBufferData::get`
    fn get<'s>(&'s self, sequence: SequenceNumber) -> &'s T { self.rb_data.get(sequence) }
    /// See `RingBufferData::take`
    unsafe fn take(&mut self, sequence: SequenceNumber) -> T { self.rb_data.take(sequence) }
    /// See `RingBufferData::unset`
    unsafe fn unset(&mut self, sequence: SequenceNumber) { self.rb_data.unset(sequence) }
    /// See `RingBufferData::is_set`
    unsafe fn is_set(&self, sequence: SequenceNumber) -> bool { self.rb_data.is_set(sequence) }
}

/**
 * Like RingBuffer, but can also allow publishers to reallocate a larger buffer and expose it to
 * consumers. The consumers retrieve the remaining items from the old buffer until they reach a
 * flagged element left by the publisher, which signals to them that they should traverse the
 * pointer and retrieve items from the next buffer from now on.
 */
struct ResizableRingBuffer<T> {
    d: UncheckedUnsafeArc<ResizableRingBufferData<T>>
}

impl<T: Send> ResizableRingBuffer<T> {
    /// Construct a new ResizableRingBuffer with a capacity for `size` elements. As with
    /// RingBuffer, `size` must be a power of two.
    fn new(size: uint) -> ResizableRingBuffer<T> {
        ResizableRingBuffer {
            d: UncheckedUnsafeArc::new(ResizableRingBufferData::new(size))
        }
    }

    /// Check for the reallocation flag in a given slot. If it is set, then access the next
    /// reference on the ResizableRingBufferData and switch to it.
    ///
    /// Returns true if a switch to a newly allocated buffer occurred.
    unsafe fn try_switch_next(&mut self, sequence: SequenceNumber) -> bool {
        if !self.d.get().is_set(sequence) {
            // Switch to newly allocated buffer
            debug!("Following switch, sequence: {:?}, unwrapped_sequence: {:?}", sequence,
                    Sequence::unwrap_number(sequence, self.size()));
            self.d = self.d.get().next.get_mut_ref().clone();
            return true;
        }
        false
    }

    /**
     * Allocates a new ring buffer of the given size, then replaces self with a reference to the
     * newly allocated buffer.
     */
    unsafe fn reallocate(&mut self, sequence: SequenceNumber, new_size: uint) {
        let new_rrbd = self.d.get().reallocate(sequence, new_size);
        self.d = new_rrbd;
    }
}

impl<T: Send> Clone for ResizableRingBuffer<T> {
    /// Copy a reference to the original buffer.
    fn clone(&self) -> ResizableRingBuffer<T> {
        ResizableRingBuffer { d: self.d.clone() }
    }
}

impl<T: Send> RingBufferOps<T> for ResizableRingBuffer<T> {
    fn size(&self) -> uint {
        unsafe {
            self.d.get_immut().size()
        }
    }
    unsafe fn set(&mut self, sequence: SequenceNumber, value: T) {
        let rrbd = self.d.get();
        rrbd.set(sequence, value);
    }
    unsafe fn get<'s>(&'s mut self, sequence: SequenceNumber) -> &'s T {
        let rrbd = self.d.get();
        rrbd.get(sequence)
    }
    unsafe fn take(&mut self, sequence: SequenceNumber) -> T {
        let rrbd = self.d.get();
        rrbd.take(sequence)
    }
}

#[test]
fn test_resizeable_ring_buffer() {
    // General smoke test
    let mut publisher_rb = ResizableRingBuffer::<uint>::new(2);
    let mut consumer_rb = publisher_rb.clone();

    // Dummy values
    let v = [123, 231, 312];

    // Hypothetical publisher writes a value, then resizes, then writes 2 more values (3 total,
    // whereas the initial buffer only holds two values)
    let mut s = SequenceNumber(0);
    let SequenceNumber(ref mut s_value) = s;
    unsafe { publisher_rb.set(s, v[0]); }
    *s_value += 1;
    unsafe { publisher_rb.reallocate(s, 4); }
    unsafe { publisher_rb.set(s, v[1]); }
    *s_value += 1;
    unsafe { publisher_rb.set(s, v[2]); }

    // Consumer gets all three values, switching to the next buffer as needed
    let mut s2 = SequenceNumber(0);
    let SequenceNumber(ref mut s2_value) = s2;
    for i in v.iter() {
        let _switch_occurred = unsafe { consumer_rb.try_switch_next(s2) };
        assert_eq!( unsafe { consumer_rb.take(s2) }, *i);
        *s2_value += 1;
    }
}

/**
 * Returns how many slots are open between the publisher's sequence and the consumer's sequence,
 * taking into account the effects of wrapping, and also resizing.
 */
fn calculate_available_publisher_resizing(
    gating_sequence: SequenceNumber,
    waiting_sequence: SequenceNumber,
    buffer_size: uint
) -> uint {
    let SequenceNumber(gating_value) = gating_sequence;
    let SequenceNumber(mut waiting_value) = waiting_sequence;
    // Handle wrapping
    if (waiting_value < gating_value) {
        waiting_value += wrap_boundary(buffer_size);
    }
    let first_unavailable_slot = gating_value + buffer_size;
    // Handle resizing
    if (first_unavailable_slot < waiting_value) {
        return 0;
    }
    let available = first_unavailable_slot - waiting_value;
    available
}

#[test]
fn test_calculate_available_publisher_resizing() {
    // Test a few in sequence:
    let test = |g, w, s | {
        calculate_available_publisher_resizing(SequenceNumber(g), SequenceNumber(w), s)
    };
    let buffer_size = 4;
    assert_eq!(test(0, 0, buffer_size), 4);
    assert_eq!(test(0, 1, buffer_size), 3);
    assert_eq!(test(0, 2, buffer_size), 2);
    // Publisher stops before using up last slot, last slot is reserved for signal
    assert_eq!(test(0, 3, buffer_size), 1);
    // Consumer catches up
    assert_eq!(test(3, 3, buffer_size), 4);
    // Publisher resizes here, new sequence value is 19 ( (6 % buffer_size) +
    // wrap_boundary(buffer_size) + 1).
    assert_eq!(test(3, 6, buffer_size), 1);
    let new_buffer_size = 8;
    assert_eq!(test(3, 19, new_buffer_size), 0);
}


/**
 * Resizing variant of SinglePublisherSequenceBarrier.
 */
struct SingleResizingPublisherSequenceBarrier<T, W> {
    // Reuse SinglePublisherSequenceBarrier data declarations and constructor
    sb: SinglePublisherSequenceBarrier<W, ResizableRingBuffer<T>>,
}

impl<T: Send, W: ResizingWaitStrategy> SingleResizingPublisherSequenceBarrier<T, W> {
    fn new(
        ring_buffer: ResizableRingBuffer<T>,
        dependencies: ~[SequenceReader],
        wait_strategy: W
    ) -> SingleResizingPublisherSequenceBarrier<T, W> {
        SingleResizingPublisherSequenceBarrier {
            sb: SinglePublisherSequenceBarrier::new(ring_buffer, dependencies, wait_strategy),
        }
    }
}

impl<T: Send, W: ResizingWaitStrategy>
        SequenceBarrier<T>
        for SingleResizingPublisherSequenceBarrier<T, W> {
    // Inherited functions
    fn get_current(&self) -> SequenceNumber { self.sb.get_current() }
    fn set_cached_available(&mut self, available: uint) { self.sb.set_cached_available(available) }
    fn get_cached_available(&self) -> uint { self.sb.get_cached_available() }
    fn release_n_real(&mut self, batch_size: uint) { self.sb.release_n_real(batch_size) }
    fn get_sequence(&self) -> SequenceReader { self.sb.get_sequence() }
    fn get_dependencies<'s>(&'s self) -> &'s [SequenceReader] { self.sb.get_dependencies() }
    fn set_dependencies(&mut self, dependencies: ~[SequenceReader]) {
        self.sb.set_dependencies(dependencies);
    }
    fn size(&self) -> uint { self.sb.size() }
    unsafe fn set(&mut self, value: T) { self.sb.set(value) }
    unsafe fn get<'s>(&'s mut self) -> &'s T { self.sb.get() }
    unsafe fn take(&mut self) -> T { self.sb.take() }

    /**
     * Wait for N slots to be available, or reallocate a larger buffer to hold it, if the resizing
     * policy requests that.
     */
    fn next_n_real(&mut self, batch_size: uint) -> uint {
        // NOTE: the returned availability value is always 1 less than the actual number of
        // available slots. The extra slot is reserved for use below with the reallocate
        // function.
        let current_size = self.sb.ring_buffer.size();
        let mut available = self.sb.wait_strategy.try_wait_for_consumers(
            batch_size, self.get_current(), self.sb.dependencies, current_size,
            calculate_available_publisher_resizing);

        if (available < batch_size) {
            // The ResizingWaitStrategy decided that we should allocate a new buffer instead of
            // waiting long enough, so we reallocate here.

            // Make the new buffer twice as large
            let new_size = 2*current_size;

            // If the sequence has been wrapped, then it is temporarily going to be less than
            // consumer sequence numbers in the other stages of the pipeline. If we allocate a
            // larger buffer and publish into it, then the publisher's sequence can overtake the
            // other sequences in the pipeline, which would break the availability calculations.
            // Unwrapping the publisher's sequence ensures that availablity calculations remain
            // correct throughout the reallocation transition.
            let old_sequence = self.get_current();
            self.sb.sequence.unwrap(current_size);
            let unwrapped_sequence = self.get_current();

            // Resizing shouldn't be a normal part of a program's operation. Alert the user, so that
            // they can consider fixing the issue.
            error!(
                "Possible deadlock detected, allocating a larger buffer for disruptor events. Current buffer size: {:?}, new size: {:?}, batch size: {:?}",
                current_size, new_size, batch_size
            );
            debug!("sequence: {:?}, unwrapped sequence: {:?}",
                    old_sequence.value(), unwrapped_sequence.value());

            unsafe {
                self.sb.ring_buffer.reallocate(unwrapped_sequence, new_size);
            }

            // Modify the cached availability value to facilitate usage of the entire newly
            // allocated buffer. The safety of this change depends on knowing that the publisher can
            // only reach the wrap boundary after the rest of the pipeline has transitioned to the
            // larger buffer. To facilitate this constraint, the wrap_boundary is 4 times the buffer
            // size, and the unwrap function leaves the sequence value in between 2 and 2.5 times
            // the new buffer size. Thus, the publisher will only wrap after publishing more than
            // buffer_size items, and that will require it to wait for consumers to catch up.
            // Alternatively, if consumers don't catch up, the publisher may reallocate another
            // buffer, but in doing so, it will continue to avoid wrapping its sequence number.
            available = new_size - 1;
        }

        available
    }
}

impl<T: Send, W: ProcessingWaitStrategy>
        NewConsumerBarrier<SingleResizingConsumerSequenceBarrier<T, W>>
        for SingleResizingPublisherSequenceBarrier<T, W> {
    fn new_consumer_barrier(&self) -> SingleResizingConsumerSequenceBarrier<T, W> {
        SingleResizingConsumerSequenceBarrier::new(self.sb.new_consumer_barrier())
    }
}

/**
 * Resizing-aware consumer barrier.
 */
struct SingleResizingConsumerSequenceBarrier<T, W> {
    /// Reuse data and constructor from SingleConsumerSequenceBarrier
    cb: SingleConsumerSequenceBarrier<W, ResizableRingBuffer<T>>
}

impl<T: Send, W: ProcessingWaitStrategy> SingleResizingConsumerSequenceBarrier<T, W> {
    fn new(
        cb: SingleConsumerSequenceBarrier<W, ResizableRingBuffer<T>>
    ) -> SingleResizingConsumerSequenceBarrier<T, W> {
        SingleResizingConsumerSequenceBarrier {
            cb: cb
        }
    }

    /**
     * Alters this barrier's sequence to follow the same path that the publisher's took when it
     * allocated a new buffer. This is necessary when following buffer reallocations to ensure
     * that downstream consumers take from the same slots that the publisher has written to.
     *
     * # Arguments
     *
     * * old_buffer_size - The ring buffer's size before the reallocation occurred.
     *
     */
    fn unwrap_sequence(&mut self, old_buffer_size: uint) {
        let original_sequence = self.get_current().value();

        self.cb.sb.sequence.unwrap(old_buffer_size);

        // The cached availability number has been artificially inflated, at this point, by the
        // publisher's sequence number being unwrapped. It needs to be adjusted to compensate for
        // this.
        //
        // Because of this adjustment, batching cannot be supported for consumers without
        // restructuring the SequenceBarrier trait's usage pattern. However, batching isn't as
        // important or necessary as it is for publishers, because the consumers are able to
        // automatically batch both reads of gating sequence values, and atomic updates of their own
        // sequence value, in between calls.
        let unwrapped_sequence = self.get_current().value();
        let current_available = self.get_cached_available();
        let unwrap_difference = (unwrapped_sequence - original_sequence);
        let mut actual_cached_available = current_available - unwrap_difference;
        // The current cached availability value may be less than the difference if the consumer's
        // sequence has wrapped since it last re-checked availability: the consumer's sequence value
        // is closer to the publisher's before the wrapping occurs, which results in a less inflated
        // availability value. In this case, the value is reset to 1, which forces the actual number
        // of available slots to be refreshed before the next item is processed.
        if current_available <= unwrap_difference {
            actual_cached_available = 1;
        }
        debug!("Adjusting available by {:?}, from {:?} to {:?}. Original sequence: {:?}, unwrapped: {:?}",
                unwrap_difference, self.get_cached_available(), actual_cached_available,
                original_sequence, unwrapped_sequence);
        self.set_cached_available(actual_cached_available);
    }

    /// Check for a reallocation flag in the slot pointed to by `sequence`. If so, adjust our
    /// sequence to match the change that would have happened to the publisher's sequence, and
    /// adjust the cached availability value to compensate for that jump.
    unsafe fn try_switch_next(&mut self) {
        let old_buffer_size = self.size();
        let old_sequence = self.get_current();
        let switch_occurred = self.cb.sb.ring_buffer.try_switch_next(old_sequence);
        if switch_occurred {
            // This is necessary to dereference the same slots that the publisher has written to.
            // In other words, downstream consumers must retrace the publisher's steps.
            self.unwrap_sequence(old_buffer_size);
        }
    }
}

impl<T: Send, W: ProcessingWaitStrategy>
        SequenceBarrier<T>
        for SingleResizingConsumerSequenceBarrier<T, W> {
    fn get_current(&self) -> SequenceNumber { self.cb.get_current() }
    fn set_cached_available(&mut self, available: uint) { self.cb.set_cached_available(available) }
    fn get_cached_available(&self) -> uint { self.cb.get_cached_available() }
    fn get_dependencies<'s>(&'s self) -> &'s [SequenceReader] { self.cb.get_dependencies() }
    fn set_dependencies(&mut self, dependencies: ~[SequenceReader]) {
        self.cb.set_dependencies(dependencies);
    }
    fn get_sequence(&self) -> SequenceReader { self.cb.get_sequence() }

    // Unfortunately, the resizing scheme removes the ability to guarantee that more than one slot
    // is actually available after returning from next_n, because the next slot could be the last
    // one that was published in the current buffer. This is fixable, but for now, just disable
    // support for larger batch sizes.
    fn next_n_real(&mut self, batch_size: uint) -> uint {
        assert!(batch_size == 1, "Batch sizes larger than 1 are currently not supported with resizable buffers.")
        self.cb.next_n_real(1)
    }
    fn release_n_real(&mut self, batch_size: uint) {
        self.cb.release_n_real(batch_size)
    }
    fn size(&self) -> uint { self.cb.size() }
    unsafe fn set(&mut self, value: T) { self.cb.set(value) }

    // The get and take functions check for reallocation events, and adjust the passed in sequence
    // and the barrier's sequence as necessary to match the adjustment to the publisher's sequence
    // that occurred at the time of reallocation.

    unsafe fn get<'s>(&'s mut self) -> &'s T {
        self.try_switch_next();
        self.cb.get()
    }
    unsafe fn take(&mut self) -> T {
        self.try_switch_next();
        self.cb.take()
    }
}

impl<T: Send, W: ProcessingWaitStrategy>
        NewConsumerBarrier<SingleResizingConsumerSequenceBarrier<T, W>>
        for SingleResizingConsumerSequenceBarrier<T, W> {
    fn new_consumer_barrier(&self) -> SingleResizingConsumerSequenceBarrier<T, W> {
        SingleResizingConsumerSequenceBarrier {
            cb: self.cb.new_consumer_barrier()
        }
    }
}

/// Default timeout, in milliseconds, after which publishers will allocate a new ring buffer instead
/// of continuing to wait. This value was chosen with a strong preference for avoiding false
/// positives.
pub static default_resize_timeout: uint = 500;

/**
 * Specialization for resizable ring buffer.
 */
impl<T: Send> Publisher<SingleResizingPublisherSequenceBarrier<T, TimeoutResizeWaitStrategy>> {
    /**
     * Create a new Publisher using a resizable ring buffer, specifying the timeout after
     * which the publisher will allocate a larger buffer to publish items into.
     *
     * # Arguments
     *
     * * resize_timeout - How long to wait, in milliseconds, before reallocating a larger buffer
     * * max_spin_tries_publisher - See YieldWaitStrategy::new_with_retry_count
     * * max_spin_tries_consumer - See YieldWaitStrategy::new_with_retry_count
     */
    pub fn new_resize_after_timeout_with_params(
        size: uint,
        resize_timeout: uint,
        max_spin_tries_publisher: uint,
        max_spin_tries_consumer: uint
    ) -> Publisher<SingleResizingPublisherSequenceBarrier<T, TimeoutResizeWaitStrategy>> {
        let ring_buffer =  ResizableRingBuffer::<T>::new(size);

        let blocking_wait_strategy = BlockingWaitStrategy::new_with_retry_count(
            max_spin_tries_publisher, max_spin_tries_consumer);
        let wait_strategy = TimeoutResizeWaitStrategy::new_with_timeout(
            resize_timeout, blocking_wait_strategy);
        let sb = SingleResizingPublisherSequenceBarrier::new(ring_buffer, ~[], wait_strategy);
        Publisher::new_common(
            sb
        )
    }

    /// Construct a TimeoutResizeWaitStrategy using the default parameters.
    pub fn new_resize_after_timeout(size: uint)
    -> Publisher<SingleResizingPublisherSequenceBarrier<T, TimeoutResizeWaitStrategy>> {

        Publisher::new_resize_after_timeout_with_params(
            size,
            default_resize_timeout,
            default_max_spin_tries_publisher,
            default_max_spin_tries_consumer
        )
    }
}

/**
 * This trait provides policy decisions regarding how long to wait before reallocating a larger
 * buffer.
 */
trait ResizingWaitStrategy : ProcessingWaitStrategy {
    /**
     * See PublishingWaitStrategy::wait_for_consumers. This function has identical semantics, except
     * that it may finish before the requested number of slots are available, returning a value that
     * is less than `n`. If this happens, the caller should reallocate a larger buffer and start
     * publishing items into that buffer instead of waiting. It also always keeps a single extra
     * slot free in the background, for use in communicating to consumers that a reallocation has
     * occurred.
     */
    fn try_wait_for_consumers(
        &self,
        n: uint,
        waiting_sequence: SequenceNumber,
        dependencies: &[SequenceReader],
        buffer_size: uint,
        calculate_available: |gating_sequence: SequenceNumber, waiting_sequence: SequenceNumber, batch_size: uint| -> uint
    ) -> uint;
}

/**
 * A wait strategy that acts like BlockingWaitStrategy, except that the publisher gives up after a
 * specified length of time and instead allocates a larger buffer to publish items into.
 *
 * Wait strategies other than BlockingWaitStrategy are meant for performance-critical applications,
 * where using automatic resizing would not have made sense. As such, there would have been little
 * value in designing this type to work with other wait strategies.
 */
struct TimeoutResizeWaitStrategy {
    /**
     * Time (in milliseconds) that the publisher should wait for the pipeline to start moving before
     * assuming that there is a deadlock and allocating a larger buffer.
     */
    timeout: uint,
    /**
     * Fallback wait strategy. See the constructor documentation for details about how it's used.
     */
    wait_strategy: BlockingWaitStrategy,
}

impl TimeoutResizeWaitStrategy {
    /**
     * Construct a new TimeoutResizeWaitStrategy, using `timeout_msecs` to decide when to resize, and
     * `wait_strategy` to implement the consumer waiting. The wait strategy is also used to
     * configure how long the publisher's task should spin when waiting for consumers, before
     * backing off to yielding.
     */
    fn new_with_timeout(timeout_msecs: uint, wait_strategy: BlockingWaitStrategy) -> TimeoutResizeWaitStrategy {
        TimeoutResizeWaitStrategy {
            timeout: timeout_msecs,
            wait_strategy: wait_strategy,
        }
    }
}

impl Clone for TimeoutResizeWaitStrategy {
    fn clone(&self) -> TimeoutResizeWaitStrategy {
        TimeoutResizeWaitStrategy {
            timeout: self.timeout,
            wait_strategy: self.wait_strategy.clone(),
        }
    }
}

impl TimeoutResizeWaitStrategy {
    /**
     * Perform the actual waiting and policy decision for
     * `ResizingWaitStrategy::try_wait_for_consumers`, but leave the issue of reserving an extra
     * slot for the caller.
     */
    fn try_wait_for_consumers_real(
        &self,
        n: uint,
        waiting_sequence: SequenceNumber,
        dependencies: &[SequenceReader],
        buffer_size: uint,
        calculate_available: |gating_sequence: SequenceNumber, waiting_sequence: SequenceNumber, batch_size: uint| -> uint
    ) -> uint {
        // Try once before querying the current time, in case the slots have become available since
        // the last wait.
        let mut available = spin_for_consumer_retries(
            n,
            waiting_sequence,
            dependencies,
            buffer_size,
            &calculate_available,
            1
        );
        if available >= n {
            return available;
        }

        // Not enough slots are available. Spin up to max_spin_tries_consumer times, then yield
        // repeatedly until either the items become available, or the timeout is reached.
        let now = precise_time_ns();
        let end_time = now + (self.timeout as u64) * 1000 * 1000;

        spin_for_consumer_retries(
            n,
            waiting_sequence,
            dependencies,
            buffer_size,
            &calculate_available,
            self.wait_strategy.max_spin_tries_consumer
        );
        if available >= n {
            return available;
        }

        while available < n && precise_time_ns() < end_time {
            available = calculate_available_list(waiting_sequence, dependencies, buffer_size,
                &calculate_available);
            task::deschedule();
        }
        // If the timeout was reached, this will be less than n
        available
    }
}

impl ResizingWaitStrategy for TimeoutResizeWaitStrategy {
    fn try_wait_for_consumers(
        &self,
        mut n: uint,
        waiting_sequence: SequenceNumber,
        dependencies: &[SequenceReader],
        buffer_size: uint,
        calculate_available: |gating_sequence: SequenceNumber, waiting_sequence: SequenceNumber, batch_size: uint| -> uint
    ) -> uint {
        // Always leave an extra slot available, for use being marked if we decide to allocate a
        // larger buffer.
        n += 1;
        let available = self.try_wait_for_consumers_real(
            n,
            waiting_sequence,
            dependencies,
            buffer_size,
            calculate_available
        );

        // Don't expose the extra slot to callers, so it remains unused
        if (available >= 1) {
            available - 1
        }
        else {
            0
        }
    }
}

impl ProcessingWaitStrategy for TimeoutResizeWaitStrategy {
    fn wait_for_publisher(
        &mut self,
        n: uint,
        waiting_sequence: SequenceNumber,
        cursor: &SequenceReader,
        buffer_size: uint
    ) -> uint {
        // Consumers wait as normal
        self.wait_strategy.wait_for_publisher(n, waiting_sequence, cursor, buffer_size)
    }
}

impl PublishingWaitStrategy for TimeoutResizeWaitStrategy {
    fn wait_for_consumers(
        &self,
        n: uint,
        waiting_sequence: SequenceNumber,
        dependencies: &[SequenceReader],
        buffer_size: uint,
        calculate_available: |gating_sequence: SequenceNumber, waiting_sequence: SequenceNumber, batch_size: uint| -> uint
    ) -> uint {
        // This code path should be unused
        self.wait_strategy.wait_for_consumers(n, waiting_sequence, dependencies, buffer_size, calculate_available)
    }

    fn notifyAllWaiters(&mut self) {
        self.wait_strategy.notifyAllWaiters();
    }
}

impl fmt::Show for TimeoutResizeWaitStrategy {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f.buf,
            "disruptor::TimeoutResizeWaitStrategy\\{t: {}, p: {}, c: {}\\}",
            self.timeout,
            self.wait_strategy.max_spin_tries_publisher,
            self.wait_strategy.max_spin_tries_consumer
        )
    }
}
