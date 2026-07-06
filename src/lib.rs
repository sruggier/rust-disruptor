// Copyright 2026 Simon Ruggier.
//
// Licensed under the Apache License, Version 2.0
// <https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <https://opensource.org/license/MIT>, at your option. This file may not be
// copied, modified, or distributed except according to those terms.

#[macro_use]
extern crate log;
use std::array::from_fn;
use std::cell::UnsafeCell;
use std::clone::Clone;
use std::cmp;
use std::fmt;
use std::hint::spin_loop;
use std::ops::Deref;
use std::ops::DerefMut;
use std::option::Option;
use std::sync::Arc;
use std::sync::atomic::Ordering::{AcqRel, Acquire, Release};
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::sync::{Condvar, Mutex};
use std::thread;
use std::time::Duration;
use std::vec::Vec;

use const_power_of_two::PowerOfTwoUsize;
use crossbeam_utils::CachePadded;
use quanta::Instant;

// To start, define the data model for the ring buffer:

/// A statically-sized buffer, with nullable entries and cache line padding.
struct RingBufferData<T, const N: usize> {
    entries: [CachePadded<T>; N],
}

impl<T, const N: usize> Default for RingBufferData<T, N>
where
    T: Default,
{
    fn default() -> Self {
        RingBufferData {
            entries: from_fn(|_| CachePadded::new(T::default())),
        }
    }
}

// Now define a dynamic version of the same thing:

/// A dynamically-sized buffer, with nullable entries and cache line padding
struct BoxedRingBufferData<T> {
    entries: Vec<CachePadded<T>>,
}

impl<T> BoxedRingBufferData<T>
where
    T: Default,
{
    /// Given a size, initialize a new instance
    fn new(size: usize) -> BoxedRingBufferData<T> {
        BoxedRingBufferData {
            entries: (0..size).map(|_| CachePadded::new(T::default())).collect(),
        }
    }
}

// Now define a distinct type, to index into the ring buffer, that assumes
// power-of-two sizes and automatically wraps.

/// Values of this object are used as indices into the ring buffer (modulo the buffer size). The
/// current value represents the latest slot that a publisher or consumer is still processing. In
/// other words, a value of 0 means that no slots have been published or consumed, and that 0 is the
/// current slot being processed. A value of 1 means that slot 0 has been released for processing by
/// downstream consumers, while a value of 18 would mean that slots 0-17 are available for
/// processing.
#[derive(Debug, Clone, Copy)]
pub struct SequenceNumber(usize);

fn assert_power_of_two(buffer_size: usize) {
    // This are redundant with other checks, and part of a potentially hot code
    // path, so debug_assert is an appropriate choice here.
    debug_assert!(
        buffer_size.is_power_of_two(),
        "Ring buffer size must be a power of two (received {})",
        buffer_size
    );
}

/// Represents an initial state where no slots have been published or consumed.
const SEQUENCE_INITIAL: usize = 0;

impl SequenceNumber {
    /// Returns self modulo `buffer_size`, exploiting the assumption that the size will always be a
    /// power of two by using a masking operation instead of the modulo operator.
    fn as_index(self, buffer_size: usize) -> usize {
        assert_power_of_two(buffer_size);
        let index_mask = buffer_size - 1;
        let SequenceNumber(value) = self;
        value & index_mask
    }

    /// Return the SequenceNumber's usize value. For when a destructuring let isn't concise enough.
    fn value(self) -> usize {
        let SequenceNumber(value) = self;
        value
    }

    /// Performs a subtraction, checking for overflow and unwrapping with respect to the given
    /// wrap_boundary. If wrap_boundary is 0, the subtraction will wrap across usize::MAX.
    fn wrapping_sub(self, rhs: usize, wrap_boundary: usize) -> SequenceNumber {
        let mut value = self.0;
        if rhs > self.0 {
            value = value.wrapping_add(wrap_boundary);
        }
        // use a normal subtraction here, opting into overflow checks in debug mode.
        value = value.wrapping_sub(rhs);
        SequenceNumber(value)
    }
}

/// Returns the number at which sequence values will be wrapped back to 0 using a mod operation.
/// The returned number will be a power of two, and a multiple of buffer_size.
fn wrap_boundary(buffer_size: usize) -> usize {
    assert_power_of_two(buffer_size);
    debug_assert!(buffer_size <= usize::MAX.div_ceil(4));
    buffer_size.wrapping_mul(4)
}

// Use a trait to abstract away the data model, so higher-level operations only need to be
// implemented once.

/// Implementation of this trait facilitates use of a given type as a ring buffer with the disruptor
/// implementation in this crate.
pub trait RingBufferAsSlice {
    /// The type to expose in higher-level interfaces
    type T;

    /// The type of each element in the returned slices.
    type Element: DerefMut<Target = Self::T>;

    /// Returns a slice containing the entire buffer.
    fn as_slice(&self) -> &[Self::Element];
    /// Returns a mutable slice containing the entire buffer.
    fn as_mut_slice(&mut self) -> &mut [Self::Element];
}

impl<T, const N: usize> RingBufferAsSlice for RingBufferData<T, N>
where
    // The blanket RingBufferOps implementation depends on the size being a power of two.
    usize: PowerOfTwoUsize<N>,
{
    type T = T;
    type Element = CachePadded<T>;

    fn as_slice(&self) -> &[CachePadded<T>] {
        &self.entries
    }

    fn as_mut_slice(&mut self) -> &mut [CachePadded<T>] {
        &mut self.entries
    }
}

/// Enables the use of a blanket RingBufferOps implementation.
impl<T> RingBufferAsSlice for BoxedRingBufferData<T> {
    type T = T;
    type Element = CachePadded<T>;

    fn as_slice(&self) -> &[CachePadded<T>] {
        &self.entries
    }
    fn as_mut_slice(&mut self) -> &mut [CachePadded<T>] {
        &mut self.entries
    }
}

// Now define some common operations that are indexed SequenceNumber.

/// Operations that index via SequenceNumber, and automatically handle wrapping.
trait RingBufferOps {
    type T;

    /// Writes a value into the ring buffer.
    ///
    /// The given sequence number is converted into an index into the buffer,
    /// and the value is moved in into that element of the buffer.
    fn set_sequence(&mut self, sequence: SequenceNumber, value: Self::T);

    /// Returns the length of the underlying buffer.
    fn len(&self) -> usize;

    /// Returns an immutable reference to the value pointed to by `sequence`.
    fn get_sequence(&self, sequence: SequenceNumber) -> &Self::T;

    /// Returns a mutable reference to the value pointed to by `sequence`.
    fn get_sequence_mut(&mut self, sequence: SequenceNumber) -> &mut Self::T;
}

// Implement the operations for each of the defined types.

/// Blanket implementation for anything that implements [`RingBufferAsSlice`].
///
/// The size is assumed to be a power of two, but that can't be enforced
/// at compile time, in general, so it's enforced via a debug assertion in
/// as_index.
impl<S, E, T> RingBufferOps for S
where
    S: RingBufferAsSlice<T = T, Element = E>,
    E: DerefMut<Target = T> + 'static,
{
    type T = T;

    fn set_sequence(&mut self, sequence: SequenceNumber, value: Self::T) {
        let index = sequence.as_index(RingBufferOps::len(self));
        *(self.as_mut_slice()[index]) = value;
    }

    fn len(&self) -> usize {
        self.as_slice().len()
    }

    fn get_sequence(&self, sequence: SequenceNumber) -> &Self::T {
        let index = sequence.as_index(RingBufferOps::len(self));
        self.as_slice()[index].deref()
    }

    fn get_sequence_mut(&mut self, sequence: SequenceNumber) -> &mut Self::T {
        let index = sequence.as_index(RingBufferOps::len(self));
        self.as_mut_slice()[index].deref_mut()
    }
}

// Everything above has been safe. Now define unsafe wrappers that allow mutable references to be
// shared across multiple owners, potentially in different threads. These unsafe types will be used
// to build a safe abstraction.

/// An unsafe reference-counted pointer that can be shared amongst multiple threads.
///
/// # Safety notes
///
/// It's the user's responsibility to synchronize access to the inner value.
#[derive(Debug)]
struct UncheckedUnsafeArc<T> {
    arc: Arc<UnsafeCell<T>>,
}

impl<T> UncheckedUnsafeArc<T> {
    fn new(data: T) -> UncheckedUnsafeArc<T> {
        let arc = Arc::new(UnsafeCell::new(data));
        UncheckedUnsafeArc { arc }
    }

    /// Gets a mutable reference to the underlying value
    ///
    /// # Safety notes
    ///
    /// It's the caller's responsibility to protect against data races.
    unsafe fn get_mut(&mut self) -> &mut T {
        unsafe { &mut *self.arc.get() }
    }

    /// Gets a reference to the underlying value
    ///
    /// # Safety notes
    ///
    /// It's the caller's responsibility to protect against data races.
    unsafe fn get(&self) -> &T {
        unsafe { &*self.arc.get() }
    }
}

impl<T> Clone for UncheckedUnsafeArc<T> {
    fn clone(&self) -> UncheckedUnsafeArc<T> {
        UncheckedUnsafeArc {
            arc: self.arc.clone(),
        }
    }
}

/// This is an implicit commitment that the thread-unsafe methods exposed from
/// this type are correctly labelled as unsafe, and adequately documented.
unsafe impl<T> Send for UncheckedUnsafeArc<T> where T: Send {}

/// A reference-counted pointer to a statically-sized circular buffer, implementing
/// [`UnsafeRingBufferDeref`].
struct RingBufferArc<T, const N: usize> {
    data: UncheckedUnsafeArc<RingBufferData<T, N>>,
}

impl<T, const N: usize> RingBufferArc<T, N>
where
    T: Default,
{
    /// Constructs a new [`RingBufferData``] with a capacity of `N` elements, and returns a
    /// reference to it. The const parameter `N` must be a power of two for the rest of the
    /// functionality in this crate to be available for use.
    fn new() -> RingBufferArc<T, N> {
        let data = RingBufferData::default();
        RingBufferArc {
            data: UncheckedUnsafeArc::new(data),
        }
    }
}

impl<T, const N: usize> Clone for RingBufferArc<T, N> {
    /// Copy a reference to the original buffer.
    fn clone(&self) -> RingBufferArc<T, N> {
        RingBufferArc {
            data: self.data.clone(),
        }
    }
}

/// Enables the use of a blanket UnsafeRingBufferDeref implementation.
impl<T, const N: usize> Deref for RingBufferArc<T, N> {
    type Target = UncheckedUnsafeArc<RingBufferData<T, N>>;

    fn deref(&self) -> &Self::Target {
        &self.data
    }
}

/// Enables the use of a blanket UnsafeRingBufferDeref implementation.
impl<T, const N: usize> DerefMut for RingBufferArc<T, N> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.data
    }
}

/// Unsafe methods providing shared references to the underlying ring buffer.
trait UnsafeRingBufferDeref {
    type RB;

    /// Get a reference to the underlying ring buffer
    ///
    /// # Safety notes
    ///
    /// It's the caller's responsibility to avoid data races when accessing
    /// elements in the buffer.
    unsafe fn get(&self) -> &Self::RB;
    /// Get a mutable reference to the underlying ring buffer
    unsafe fn get_mut(&mut self) -> &mut Self::RB;
}

/// Blanket impl for UncheckedUnsafeArc holding a RingBufferOps type.
impl<RB> UnsafeRingBufferDeref for UncheckedUnsafeArc<RB> {
    type RB = RB;

    // The duplication here is intentional: the underlying UncheckedUnsafeArc
    // type isn't specific to this use case, so the definition shouldn't be
    // coupled to it.
    unsafe fn get(&self) -> &Self::RB {
        unsafe { UncheckedUnsafeArc::get(self) }
    }
    unsafe fn get_mut(&mut self) -> &mut Self::RB {
        unsafe { UncheckedUnsafeArc::get_mut(self) }
    }
}

/// Blanket impl for types that deref to UnsafeRingBufferDeref
impl<RB, A, AD> UnsafeRingBufferDeref for AD
where
    A: UnsafeRingBufferDeref<RB = RB> + 'static,
    AD: DerefMut<Target = A>,
{
    type RB = RB;

    unsafe fn get(&self) -> &Self::RB {
        unsafe { self.deref().get() }
    }
    unsafe fn get_mut(&mut self) -> &mut Self::RB {
        unsafe { self.deref_mut().get_mut() }
    }
}

/// An unsafe version of RingBufferOps. Defines the same abstraction, but
/// without exposing it to safe code.
trait UnsafeRingBufferOps {
    type T;

    /// See `RingBufferOps::len`
    fn len(&self) -> usize;

    /// See `RingBufferOps::set`
    ///
    /// # Safety notes
    ///
    /// It's the caller's responsibility to avoid data races, so this function is unsafe.
    unsafe fn set(&mut self, sequence: SequenceNumber, value: Self::T);

    /// See `RingBufferOps::get`. Unsafe: allows data races.
    unsafe fn get(&self, sequence: SequenceNumber) -> &Self::T;
}

/// Blanket impl for types that implement UnsafeRingBufferDeref.
impl<T, SRB, URB> UnsafeRingBufferOps for URB
where
    SRB: RingBufferOps<T = T> + 'static,
    URB: UnsafeRingBufferDeref<RB = SRB>,
{
    type T = T;

    fn len(&self) -> usize {
        unsafe { self.get().len() }
    }
    unsafe fn set(&mut self, sequence: SequenceNumber, value: Self::T) {
        unsafe {
            self.get_mut().set_sequence(sequence, value);
        }
    }
    unsafe fn get(&self, sequence: SequenceNumber) -> &Self::T {
        unsafe { UnsafeRingBufferDeref::get(self).get_sequence(sequence) }
    }
}

/// Extends UnsafeRingBufferOps with a take function
trait UnsafeRingBufferOpsTake: UnsafeRingBufferOps {
    /// See `RingBufferOpsTake::take`. Unsafe: allows data races.
    unsafe fn take(&mut self, sequence: SequenceNumber) -> Self::T;
}

/// Blanket impl for UnsafeRingBufferDeref types, where the underlying ring buffer implements
/// RingBufferOpsTake.
impl<T, SRB, URB> UnsafeRingBufferOpsTake for URB
where
    T: Default,
    SRB: RingBufferOps<T = T> + 'static,
    URB: UnsafeRingBufferDeref<RB = SRB>,
{
    unsafe fn take(&mut self, sequence: SequenceNumber) -> Self::T {
        unsafe { std::mem::take(self.get_mut().get_sequence_mut(sequence)) }
    }
}

#[cfg(test)]
#[test_log::test]
fn ring_buffer_size_must_be_power_of_two_1() {
    RingBufferArc::<(), 1>::new();
}
#[cfg(test)]
#[test_log::test]
fn ring_buffer_size_must_be_power_of_two_8() {
    RingBufferArc::<(), 8>::new();
}

/// Returns how many slots are open between the publisher's sequence and the consumer's sequence,
/// taking into account the effects of wrapping. In this case, the waiting task is a consumer, and
/// the gating task is either a consumer or a publisher. If the gating sequence (in other words, the
/// publisher or upstream consumer) has the same value as the waiting sequence (the consumer), then
/// no slots are available for consumption.
fn calculate_available_consumer(
    gating_sequence: SequenceNumber,
    waiting_sequence: SequenceNumber,
    buffer_size: usize,
) -> usize {
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
    // assert!(available <= buffer_size, "available: {}, gating: {}, waiting: {}", available, gating, waiting);
    #[allow(clippy::let_and_return)]
    available
}

#[cfg(test)]
#[test_log::test]
fn test_calculate_available_consumer() {
    // A consumer waiting for publisher or earlier consumer
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

/// Returns how many slots are open between the publisher's sequence and the consumer's sequence,
/// taking into account the effects of wrapping.
fn calculate_available_publisher(
    gating_sequence: SequenceNumber,
    waiting_sequence: SequenceNumber,
    buffer_size: usize,
) -> usize {
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

#[cfg(test)]
#[test_log::test]
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

/// A reference to an atomic usize that allows the owner to mutate it, and can generate read-only
/// references using [`clone_immut`](SequenceOwner::clone_immut). Returns values as SequenceNumber
/// to disambiguate from indices and other usize values. Memory is managed via reference counting.
#[derive(Debug)]
struct SequenceOwner {
    /// The published value of the sequence, visible to waiting consumers.
    value_arc: UncheckedUnsafeArc<CachePadded<AtomicUsize>>,
    /// A cached copy of the sequence for use by the owner, to minimize atomic operations and
    /// contention.
    private_value: usize,
}

impl Default for SequenceOwner {
    /// Calls Self::new()
    fn default() -> Self {
        Self::new()
    }
}

/// Common implementation of get for [`SequenceOwner`] and [`SequenceReader`].
fn common_sequence_owner_get(
    value_arc: &UncheckedUnsafeArc<CachePadded<AtomicUsize>>,
) -> SequenceNumber {
    // SAFETY: SequenceOwner and SequenceReader always access the value atomically.
    unsafe { SequenceNumber(value_arc.get().load(Acquire)) }
}

impl SequenceOwner {
    /// Allocates a new sequence.
    fn new() -> Self {
        Self::new_from_sequence(SequenceNumber(SEQUENCE_INITIAL))
    }

    fn new_from_sequence(sequence: SequenceNumber) -> Self {
        SequenceOwner {
            value_arc: UncheckedUnsafeArc::new(CachePadded::new(AtomicUsize::new(
                sequence.value(),
            ))),
            private_value: sequence.value(),
        }
    }

    // used in the tests below
    #[allow(dead_code)]
    /// See [`SequenceReader::get`] method
    fn get(&self) -> SequenceNumber {
        common_sequence_owner_get(&self.value_arc)
    }

    /// Gets the internally cached value of the Sequence. This should only be called from the task
    /// that owns the sequence number (in other words, the only task that writes to the sequence
    /// number)
    fn get_owned(&self) -> SequenceNumber {
        SequenceNumber(self.private_value)
    }

    /// Return an immutable reference to the same underlying sequence number.
    fn clone_immut(&self) -> SequenceReader {
        SequenceReader {
            sequence_arc: self.value_arc.clone(),
        }
    }

    /// Add n to the cached, private version of the sequence, without making the new value visible to
    /// other threads.
    ///
    /// To avoid overflow when usize is 32 bits wide, this function also wraps the sequence number
    /// around when it reaches wrap_boundary(buffer_size). This results in two easily distinguishable
    /// states for the availability calculations to handle. Consumer sequences are normally behind
    /// gating sequences, whether they are owned by other consumers or the publisher. However, the
    /// gating sequence will wrap first, and remain behind until consumers reach the wrapping
    /// boundary, at which point they will also wrap. The publisher is normally ahead of the sequence
    /// it depends on, but after wrapping, it will be temporarily behind the gating sequence.
    fn advance(&mut self, n: usize, buffer_size: usize) {
        self.private_value = self.private_value.wrapping_add(n);
        // Given that buffer_size is a power of two, wrap by masking out the high bits. This
        // operation is a noop if the value is less than wrap_boundary(buffer_size), so it's
        // unnecessary to check before wrapping.
        let wrap_mask = wrap_boundary(buffer_size).wrapping_sub(1);
        self.private_value &= wrap_mask;
    }

    ///  Publishes the private sequence value to other threads, along with any other writes (for
    ///  example, to the corresponding item in the ring buffer) that have taken place before the
    ///  call.
    fn flush(&mut self) {
        // SAFETY: SequenceOwner ensures the value field is always accessed atomically, and
        // the private_value field is only accessed by a single owner.
        let value = unsafe { self.value_arc.get_mut() };
        value.store(self.private_value, Release);
    }

    /// Advance, then immediately make the change visible to other threads.
    fn advance_and_flush(&mut self, n: usize, buffer_size: usize) {
        self.advance(n, buffer_size);
        self.flush();
    }

    /// Reverses the effects of wrapping that occur in the advance function.
    fn unwrap(&mut self, buffer_size: usize) {
        let SequenceNumber(unwrapped) =
            SequenceOwner::unwrap_number(SequenceNumber(self.private_value), buffer_size);
        self.private_value = unwrapped;
    }

    /// Like unwrap, but for standalone SequenceNumber values.
    fn unwrap_number(sn: SequenceNumber, buffer_size: usize) -> SequenceNumber {
        let SequenceNumber(value) = sn;
        assert!(value < wrap_boundary(buffer_size));
        // We know the sequence value is in the interval [0, 4*buffer_size). This expression
        // ensures that it will be within [4*buffer_size, 5*buffer_size) instead.
        let buffer_size_mask = buffer_size - 1;
        let new_value = (value & buffer_size_mask) + wrap_boundary(buffer_size);
        SequenceNumber(new_value)
    }
}

// For now, this is only used by the test code below, so conditionally compile it
#[cfg(test)]
fn log2(mut power_of_2: usize) -> usize {
    assert!(
        power_of_2.count_ones() == 1,
        "Argument must be a power of two (received {})",
        power_of_2
    );
    let mut exp = 0;
    while power_of_2 > 1 {
        exp += 1;
        power_of_2 >>= 1;
    }
    exp
}

/// Ensure sequences correctly support the maximum buffer size.
#[cfg(test)]
#[test_log::test]
fn test_sequence_overflow() {
    // The maximum buffer size is (usize::MAX+1) / wrap_boundary(1) (for example, 2^30 with the
    // current boundary of 4*buffer_size). For that size, wrap_boundary(buffer_size) - 1 would
    // evaluate to usize::MAX, and unsigned integer arithmetic will naturally take care of the
    // wrapping. The sequence will wrap to 0 at wrap_boundary(buffer_size), i.e. usize::MAX + 1.
    let exp = log2(wrap_boundary(1));
    let max_buffer_size = 1 << (std::mem::size_of::<usize>() * 8 - exp);

    let mut s = SequenceOwner::new();
    assert_eq!(s.get().value(), SEQUENCE_INITIAL);

    // Add 1
    s.advance_and_flush(1, max_buffer_size);
    let incremented_value = s.get().value();
    assert_eq!(incremented_value, SEQUENCE_INITIAL + 1);

    // Advance to max_buffer_size
    s.advance_and_flush(max_buffer_size - incremented_value, max_buffer_size);
    assert_eq!(s.get().value(), max_buffer_size);

    // Overflow to 4*max_buffer_size + 1 and confirm that it wrapped to 1
    s.advance_and_flush(3 * max_buffer_size + 1, max_buffer_size);
    assert_eq!(s.get().value(), 1);
}

/// Immutable reference to a sequence. Can be safely given to other tasks. Reads with acquire
/// semantics.
#[derive(Debug)]
pub struct SequenceReader {
    sequence_arc: UncheckedUnsafeArc<CachePadded<AtomicUsize>>,
}

impl SequenceReader {
    /// Gets the value of the sequence, using acquire semantics. For use by publishers/consumers to
    /// confirm that slots have been released by the task(s) ahead of them in the pipeline.
    pub fn get(&self) -> SequenceNumber {
        common_sequence_owner_get(&self.sequence_arc)
    }
    /// Get another reference to the sequence.
    pub fn clone_immut(&self) -> SequenceReader {
        SequenceReader {
            sequence_arc: self.sequence_arc.clone(),
        }
    }
}

impl Clone for SequenceReader {
    fn clone(&self) -> Self {
        self.clone_immut()
    }
}

#[cfg(test)]
#[test_log::test]
fn test_sequencereader() {
    // For the purposes of this test, it doesn't matter what the buffer size is, as long as it's
    // larger than the tested sequence numbers
    let buffer_size = 8192;

    let mut sequence = SequenceOwner::new();
    let reader = sequence.clone_immut();
    assert!(0 == reader.get().value());
    sequence.advance_and_flush(1, buffer_size);
    assert!(1 == reader.get().value());
    sequence.advance_and_flush(11, buffer_size);
    assert!(12 == reader.get().value());
}

// Create a shorthand for availability calculation functions, since this gets repeated several
// times below.
pub trait AvailabilityFn: Fn(SequenceNumber, SequenceNumber, usize) -> usize {}
impl<F> AvailabilityFn for F where F: Fn(SequenceNumber, SequenceNumber, usize) -> usize {}

/// Given a list of dependencies, retrieves the current value of each and returns the minimum number
/// of available items out of all the dependencies.
fn calculate_available_list<F>(
    waiting_sequence: SequenceNumber,
    dependencies: &[SequenceReader],
    buffer_size: usize,
    calculate_available: &F,
) -> usize
where
    F: AvailabilityFn,
{
    if dependencies.is_empty() {
        // The corresponding stage of the pipeline has ownership of all elements in the buffer. In
        // practice, this can only happen with the publisher, and only if the caller starts setting
        // or mutating elements before constructing the rest of the pipeline.
        cmp::max(0, buffer_size - waiting_sequence.value())
    } else {
        let mut available = usize::MAX;
        for consumer_sequence in dependencies.iter() {
            let a = calculate_available(consumer_sequence.get(), waiting_sequence, buffer_size);
            available = cmp::min(available, a);
        }
        available
    }
}

// Abstraction over availability calculation, allowing the upstream pipeline stage to define the
// synchronization protocol it'll use.
pub trait PollableDependency: Send {
    fn calculate_available(&self, waiting_sequence: SequenceNumber, buffer_size: usize) -> usize;
}

/// A list of consumer sequences, used by the publisher stage to calculate availability.
#[derive(Clone, Debug, Default)]
struct PublisherDependencies {
    sequences: Vec<SequenceReader>,
}

impl PollableDependency for PublisherDependencies {
    fn calculate_available(&self, waiting_sequence: SequenceNumber, buffer_size: usize) -> usize {
        calculate_available_list(
            waiting_sequence,
            self.sequences.as_slice(),
            buffer_size,
            &calculate_available_publisher,
        )
    }
}

// A list of publisher or consumer sequences, used by consumer stages to calculate availability.
#[derive(Clone, Debug, Default)]
struct ConsumerDependencies {
    sequences: Vec<SequenceReader>,
}

impl ConsumerDependencies {
    fn from_vec(sequences: Vec<SequenceReader>) -> Self {
        ConsumerDependencies { sequences }
    }
}

impl PollableDependency for ConsumerDependencies {
    fn calculate_available(&self, waiting_sequence: SequenceNumber, buffer_size: usize) -> usize {
        calculate_available_list(
            waiting_sequence,
            self.sequences.as_slice(),
            buffer_size,
            &calculate_available_consumer,
        )
    }
}

/// Separate struct definition for the reference to the publisher, which can only refer to a single
/// SequenceReader, for now. This will be revisited when implementing support for concurrent
/// publishing.
struct PublisherAvailability {
    sequence: SequenceReader,
}

impl PollableDependency for PublisherAvailability {
    fn calculate_available(&self, waiting_sequence: SequenceNumber, buffer_size: usize) -> usize {
        calculate_available_consumer(self.sequence.get(), waiting_sequence, buffer_size)
    }
}

/// A helper trait for expressing availability.
///
/// This is defined separately because all of the other pipeline related functionality depends on
/// it. Any other function defined alongside `len_available` would be pulled into the bottom of the
/// dependency tree of various other traits.
pub trait LenAvailable {
    /// Returns the number of slots in the buffer known to be available for handling by the
    /// corresponding stage of the pipeline associated with [`self`], as of the last call to
    /// [`Pollable::poll`].
    ///
    /// # Performance
    ///
    /// Generally uses cached information, and should be cheap to call.
    fn len_available(&self) -> usize;
}

/// The minimal functionality needed to implement polling wait strategies.
pub trait Pollable: LenAvailable {
    /// Synchronize with dependencies to gain an updated view of how many slots in the buffer are
    /// available for the owner to reference. A call to this method may increase the number of
    /// elements returned by the [`len_available`](Self::len_available) method, and can be retried
    /// indefinitely until that happens.
    ///
    /// # Performance
    ///
    /// This method generally uses one or more atomic operations (with [`Acquire`] ordering) to
    /// retrieve the state of upstream dependencies, with the associated implications for
    /// performance and safety.
    ///
    /// # Safety
    ///
    /// A happens-before relationship is established between the last pipeline stage and this one,
    /// ensuring that any modifications made during the previous stage of the pipeline will be
    /// visible through this reference.
    fn poll(&mut self);
}

trait PipelineCapacity {
    /// Returns the size of the underlying ring buffer.
    fn capacity(&self) -> usize;
}

trait ReleaseSlots: LenAvailable {
    /// Release the given number of slots to downstream stages of the pipeline.
    ///
    /// # Safety
    ///
    /// 1. `n` must be less than the number of available slots, as expressed by
    ///    [`Self::len_available`], or [undefined behaviour] will result.
    ///
    /// [undefined behaviour]: https://doc.rust-lang.org/reference/behavior-considered-undefined.html
    unsafe fn release_slots_unchecked(&mut self, n: usize);
}

/// Defines a way to see the current sequence number of a reference into a pipeline.
///
/// This is generally an implementation detail, so its use should be avoided, where possible.
trait CurrentSequence {
    /// Get the current sequence number associated with this reference into a pipeline.
    fn current_sequence(&self) -> SequenceNumber;
}

/// A convenience trait that allows a type to implement pipeline-referencing traits, like
/// [`LenAvailable`], [`Pollable`], and so on, by returning a reference to some other type that
/// implements it. This can be used to delegate implementation to a field.
trait AsPipelineRef {
    /// The delegated-to type.
    type T;

    /// Return a reference to the value whose implementation of Pollable should be reused.
    fn as_pipeline_ref(&self) -> &Self::T;
    /// Mutable variant of [`Self::as_pollable`].
    fn as_pipeline_ref_mut(&mut self) -> &mut Self::T;
}

impl<D> LenAvailable for D
where
    D: AsPipelineRef,
    D::T: LenAvailable,
{
    fn len_available(&self) -> usize {
        self.as_pipeline_ref().len_available()
    }
}

/// Automatic [`Pollable`] impl for types that implement
/// AsPipelineRef.
impl<D> Pollable for D
where
    D: AsPipelineRef,
    D::T: Pollable,
{
    fn poll(&mut self) {
        self.as_pipeline_ref_mut().poll();
    }
}

impl<D> PipelineCapacity for D
where
    D: AsPipelineRef,
    D::T: PipelineCapacity,
{
    fn capacity(&self) -> usize {
        self.as_pipeline_ref().capacity()
    }
}

/// A trait for types that implement [`AsPipelineRef`]. Implementing the trait opts into a blanket
/// [`ReleaseSlots`] impl, which reuses the implementation from [`AsPipelineRef::T`].
trait DelegateReleaseSlots {}

/// Automatic [`Pollable`] impl for types that implement
/// AsPipelineRef.
impl<D> ReleaseSlots for D
where
    D: AsPipelineRef,
    D::T: ReleaseSlots,
    D: DelegateReleaseSlots,
{
    unsafe fn release_slots_unchecked(&mut self, n: usize) {
        // SAFETY: delegated to the caller.
        unsafe { self.as_pipeline_ref_mut().release_slots_unchecked(n) };
    }
}

impl<D> CurrentSequence for D
where
    D: AsPipelineRef,
    D::T: CurrentSequence,
{
    fn current_sequence(&self) -> SequenceNumber {
        self.as_pipeline_ref().current_sequence()
    }
}

/// Extension trait for types that implement PollableDependency, which allows constructing adapters
/// that take a borrowed reference, along with the required method arguments, and uses them to
/// implement [`Pollable`].
trait AsPollableFromPollableDependency<'a> {
    type T: Pollable;

    /// Return a value that implements [`Pollable`], using a shared reference to `self`.
    fn as_pollable(&'a self, waiting_sequence: SequenceNumber, buffer_size: usize) -> Self::T;
}

/// Implements currying and result caching for a reference to a type that implements
/// PollableDependency, adapting it into an implementation of Pollable.
struct PollableDependencyAsPollable<'a, P> {
    dependency: &'a P,
    waiting_sequence: SequenceNumber,
    buffer_size: usize,
    cached_available: usize,
}

impl<P> LenAvailable for PollableDependencyAsPollable<'_, P> {
    fn len_available(&self) -> usize {
        self.cached_available
    }
}

impl<P> Pollable for PollableDependencyAsPollable<'_, P>
where
    P: PollableDependency,
{
    fn poll(&mut self) {
        self.cached_available = self
            .dependency
            .calculate_available(self.waiting_sequence, self.buffer_size);
    }
}

impl<'a, P> AsPollableFromPollableDependency<'a> for P
where
    P: PollableDependency + 'a,
{
    type T = PollableDependencyAsPollable<'a, P>;

    fn as_pollable(&'a self, waiting_sequence: SequenceNumber, buffer_size: usize) -> Self::T {
        PollableDependencyAsPollable {
            dependency: self,
            waiting_sequence,
            buffer_size,
            cached_available: 0,
        }
    }
}

/// Allows waiting for upstream dependencies.
pub trait PollingWaitStrategy: Clone + Send {
    /// Wait for upstream consumers to finish processing items that have already been published,
    /// until at least min_available items are available.
    fn wait_for_dependencies(&self, pollable: &mut dyn Pollable, min_available: usize);
}

/// Interface for wait strategies that require notification when the publishing stage releases new
/// slots into the pipeline, such as [`BlockingWaitStrategy`].
///
/// This is defined separately because it imposes an additional cost on implementing types, who have
/// to query which slots have been released by the publisher to know whether it makes sense to wait
/// for a notification or not.
pub trait NotificationWaitStrategy: PollingWaitStrategy {
    /// Wait for the publisher to release the next `min_available` slots, then return the actual
    /// number of available slots, which may be greater than `min_available`.
    ///
    /// For strategies that block, only the publisher will attempt to wake the task, so this method
    /// only waits for the publisher. Consumers will also have to busy-wait on its immediate
    /// dependencies for the event to become available for processing. Once the publisher has
    /// released the necessary slots, the rest of the pipeline should release them in a bounded
    /// amount of time, so the cost of polling is less of a problem.
    fn wait_for_publisher(&mut self, pollable: &mut dyn Pollable, min_available: usize);

    /// Wakes up any consumers that have blocked waiting for new items to be published.
    ///
    /// # Safety notes
    ///
    /// This must be called only after signalling that the slot is published, or it will not always
    /// work, and consumers waiting using a blocking wait strategy may sleep indefinitely (until a
    /// second item is published).
    fn notify_all_waiters(&mut self);
}

/// Spin on a [`Pollable`] implementation until either the desired number of elements becomes
/// available, or the given maximum number of retries is reached.
///
/// If `None` is passed as an argument for the `max_tries` parameter, this function will spin
/// forever.
///
/// The function `busy_fn` will be called during each iteration, and can be used to implement
/// various back-off strategies.
fn spin_for_pollable_with_retries<F>(
    pollable: &mut dyn Pollable,
    min_available: usize,
    max_tries: Option<usize>,
    mut busy_fn: F,
) where
    F: FnMut(usize),
{
    if pollable.len_available() >= min_available {
        return;
    }

    let mut tries = 0;
    while max_tries.is_none_or(|max_tries| tries < max_tries) {
        pollable.poll();
        tries += 1;
        if pollable.len_available() >= min_available {
            return;
        }
        busy_fn(tries);
    }
}

/// Waits using simple busy waiting.
///
/// # Safety notes
///
/// Using this strategy can result in livelock when used with tasks spawned using default scheduler
/// options. Ensure all publishers and consumers are on separate OS threads when using this.
#[derive(Clone, Copy, Debug)]
pub struct SpinWaitStrategy;

impl NotificationWaitStrategy for SpinWaitStrategy {
    fn wait_for_publisher(&mut self, pollable: &mut dyn Pollable, min_available: usize) {
        spin_for_pollable_with_retries(pollable, min_available, None, |_| spin_loop())
    }

    fn notify_all_waiters(&mut self) {}
}
impl PollingWaitStrategy for SpinWaitStrategy {
    fn wait_for_dependencies(&self, pollable: &mut dyn Pollable, min_available: usize) {
        spin_for_pollable_with_retries(pollable, min_available, None, |_| spin_loop())
    }
}

pub const DEFAULT_MAX_SPIN_TRIES_PUBLISHER: usize = 2500;
pub const DEFAULT_MAX_SPIN_TRIES_CONSUMER: usize = 2500;

/// A wait strategy for use cases where high throughput and low latency are a priority, but it is
/// also desirable to avoid starving other tasks, such as when there are more tasks than CPU cores.
/// Spins for a small number of retries, then yields to other tasks repeatedly until enough items are
/// released. This will almost always be a better choice than SpinWaitStrategy, except in cases where
/// latency is paramount, and the caller has taken steps to pin the publisher and consumers to their
/// own threads, or even cores.
#[derive(Clone, Copy)]
pub struct YieldWaitStrategy {
    max_spin_tries_publisher: usize,
    max_spin_tries_consumer: usize,
}

impl Default for YieldWaitStrategy {
    /// Calls Self::new()
    fn default() -> Self {
        Self::new()
    }
}

impl YieldWaitStrategy {
    /// Create a YieldWaitStrategy that will spin for the default number of times before yielding.
    pub fn new() -> YieldWaitStrategy {
        YieldWaitStrategy::new_with_retry_count(
            DEFAULT_MAX_SPIN_TRIES_PUBLISHER,
            DEFAULT_MAX_SPIN_TRIES_CONSUMER,
        )
    }

    /// Create a YieldWaitStrategy, explicitly specifying how many times to spin before
    /// transitioning to a yielding strategy.
    ///
    /// # Arguments
    ///
    /// The two arguments represent the maximum number of times to spin while waiting for the
    /// publisher or other consumers. This is a tradeoff: one gains lower latency and increased
    /// throughput, at the expense of wasted CPU cycles.  When the CPU is oversubscribed, though,
    /// more retries could actually reduce throughput. The increased power usage is also undesirable
    /// in general. The ideal value depends on how important reduced latency and/or increased
    /// throughput are to a given use case, how frequently items are published, and how quickly
    /// consumers process new items.
    pub fn new_with_retry_count(
        max_spin_tries_publisher: usize,
        max_spin_tries_consumer: usize,
    ) -> YieldWaitStrategy {
        YieldWaitStrategy {
            max_spin_tries_publisher,
            max_spin_tries_consumer,
        }
    }
}

impl PollingWaitStrategy for YieldWaitStrategy {
    fn wait_for_dependencies(&self, pollable: &mut dyn Pollable, min_available: usize) {
        spin_for_pollable_with_retries(pollable, min_available, None, |tries| {
            if tries < self.max_spin_tries_consumer {
                spin_loop();
            } else {
                thread::yield_now();
            }
        })
    }
}

impl NotificationWaitStrategy for YieldWaitStrategy {
    fn wait_for_publisher(&mut self, pollable: &mut dyn Pollable, min_available: usize) {
        spin_for_pollable_with_retries(pollable, min_available, None, |tries| {
            if tries < self.max_spin_tries_publisher {
                spin_loop();
            } else {
                thread::yield_now();
            }
        })
    }
    fn notify_all_waiters(&mut self) {}
}

impl fmt::Debug for YieldWaitStrategy {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "YieldWaitStrategy{{p: {}, c: {}}}",
            self.max_spin_tries_publisher, self.max_spin_tries_consumer
        )
    }
}

/// Spins for a short time, then sleeps on a wait condition until the publisher signals. This comes
/// at a cost, however: the publisher has to perform an extra read-modify-write operation on a shared
/// atomic variable every time it publishes new items. The operation should be uncontended, unless it
/// happens at the same time that a waiter is about to fall asleep. See below for details and a proof
/// of correctness.
///
/// # Design issues
///
/// When a waiting task goes to sleep, it cannot sleep without a timeout unless it is certain that
/// the publisher will wake it up when the next slot is published. The conventional solution to this
/// problem would be to have the publisher acquire a lock after every publish, to guarantee that
/// waiting consumers are woken up immediately. This would impose a prohibitive performance penalty
/// if it happened here. In the common case, where the publisher does not need to signal, it would be
/// good to avoid using locks altogether. However, any alternative solutions need to make the
/// following guarantees:
///  - If a consumer decides to wait, the publisher must signal when it releases the slot that the
///    consumer was waiting for
///  - The publisher must signal _after_ the consumer has fallen asleep, or the consumer will not be
///    woken up
///
/// # Approach
///
/// We need a way for the publisher to synchronize with potential waiters at the point where it
/// checks if it needs to signal or not. If it finds that it does not need to signal, we need to be
/// able to prove that the consumer will not sleep. If it does see a need to signal, then it must be
/// assured that it will do so after the consumer has fallen asleep.
///
/// # Algorithm
///
/// The publisher executes the following steps whenever releasing items:
///  - Release items via an atomic operation with release semantics (before calling notify_all_waiters)
///  - Check if there are any waiters using a read-modify-write operation on a shared variable (with
///    rel-acq semantics)
///  - If so, acquire the lock and signal on the wait condition
///
/// The consumer executes the following steps before going to sleep:
///  - Acquire the lock
///  - Express an intent to sleep using a read-modify-write operation (with rel-acq semantics) on the
///    shared variable
///  - Check for any newly released items
///  - If none, go to sleep, otherwise release the lock and finish waiting
///
/// # Proof of correctness
///
/// Two things need to be proven to show correctness:
///  - the publisher will signal when necessary
///  - the signal happens only after the waiter(s) have gone to sleep.
///
/// Due to the use of read-modify-write operations, the race between publisher and consumer to access
/// the shared variable has two simple outcomes: either the publisher checks first and refrains from
/// signalling, or the consumer signals intent to sleep first, which the publisher will then see and
/// act upon.
///
/// If the publisher accesses the shared variable before consumer has signalled intent to sleep: as
/// long as the item has been released before the access, we can say that:
///  - the item release happens before the publisher's shared variable access
///  - which happens before the consumer's shared variable access
///  - which happens before the consumer's final check for a newly released item before sleeping
///
/// Therefore, if the publisher concludes that it doesn't need to signal, it is certain that the
/// consumer will see the newly released item and refrain from sleeping. In other words, a consumer
/// cannot go to sleep without the publisher seeing its intent to do so.
///
/// An alternate proof based on the consumer deciding to sleep: due to acquire semantics on the
/// shared variable access, we know that the consumer accesses the shared variable before checking
/// for available items.  If the consumer, after taking the lock but before going to sleep, doesn't
/// see a newly released item, the following happens-before ordering is implied: consumer shared
/// variable access -> consumer check for new item -> publisher release of new item -> publisher
/// access of shared variable. Therefore, if the waiter decides to sleep after seeing no progress
/// from the publisher, we can say for sure that the publisher will see the signalled intent to
/// sleep, acquire the lock, and signal on the wait condition.
///
/// Regarding the second issue of whether the waiter goes to sleep before the signal wakes it up,
/// observe that the consumer only expresses intent to sleep (through the shared variable) after
/// acquiring the lock. Thus, the producer cannot see the signal until after the consumer acquires
/// the lock, and cannot acquire the lock and signal until after the consumer has atomically released
/// the lock and slept.
///
/// Unfortunately, this proof depends on having release semantics on the publisher side, and acquire
/// semantics on the waiting side. Release semantics require a store operation, while Acquire
/// semantics require a load operation. Therefore, the publisher needs to load the shared variable to
/// see if it needs to signal or not, while simultaneously storing to it in order to gain release
/// semantics.  Likewise, in addition to modifying the shared variable to signal intent, the waiters
/// need to also perform a load in order to have acquire semantics. This is why read-modify-write
/// operations are needed, and not just the cheaper load/store operations.
pub struct BlockingWaitStrategy {
    d: UncheckedUnsafeArc<BlockingWaitStrategyData>,
    // Deep copyable fields that don't need to be shared
    /// Number of times to wait before blocking
    max_spin_tries_publisher: usize,
    /// Number of times to wait for a consumer sequence before yielding
    max_spin_tries_consumer: usize,
}

struct BlockingWaitStrategyData {
    /// True if any tasks are waiting for new slots to be released by the publisher.
    signal_needed: AtomicBool,
    /// Waiting consumers block on, and are signalled by, this. The data guarded by the mutex is
    /// completely superfluous and unused.
    wait_mutex: Mutex<bool>,
    wait_condvar: Condvar,
}

impl Default for BlockingWaitStrategy {
    /// Calls Self::new()
    fn default() -> Self {
        Self::new()
    }
}

impl BlockingWaitStrategy {
    pub fn new() -> BlockingWaitStrategy {
        BlockingWaitStrategy::new_with_retry_count(
            DEFAULT_MAX_SPIN_TRIES_PUBLISHER,
            DEFAULT_MAX_SPIN_TRIES_CONSUMER,
        )
    }

    /// Create a `BlockingWaitStrategy`, explicitly specifying how many times to spin before
    /// transitioning to a yielding strategy.
    ///
    /// # Arguments
    ///
    /// See `YieldWaitStrategy::new_with_retry_count` for a more detailed description of what the
    /// arguments mean. This wait strategy will block instead of yielding when the maximum number of
    /// retries is reached while waiting for the publisher.
    pub fn new_with_retry_count(
        max_spin_tries_publisher: usize,
        max_spin_tries_consumer: usize,
    ) -> BlockingWaitStrategy {
        let d = BlockingWaitStrategyData {
            signal_needed: AtomicBool::new(false),
            wait_mutex: Mutex::new(false),
            wait_condvar: Condvar::new(),
        };
        BlockingWaitStrategy {
            d: UncheckedUnsafeArc::new(d),
            max_spin_tries_publisher,
            max_spin_tries_consumer,
        }
    }
}

impl Clone for BlockingWaitStrategy {
    /// Returns a shallow copy, that waits and signals on the same wait condition.
    fn clone(&self) -> BlockingWaitStrategy {
        BlockingWaitStrategy {
            d: self.d.clone(),
            max_spin_tries_publisher: self.max_spin_tries_publisher,
            max_spin_tries_consumer: self.max_spin_tries_consumer,
        }
    }
}

impl NotificationWaitStrategy for BlockingWaitStrategy {
    fn wait_for_publisher(&mut self, pollable: &mut dyn Pollable, min_available: usize) {
        spin_for_pollable_with_retries(
            pollable,
            min_available,
            Some(self.max_spin_tries_publisher),
            |_| spin_loop(),
        );

        if pollable.len_available() >= min_available {
            return;
        }

        // Transition to blocking on wait condition
        let d;
        unsafe {
            d = self.d.get_mut();
        }

        // Grab lock on wait condition
        let signal_needed = &mut d.signal_needed;
        {
            while min_available > pollable.len_available() {
                let mutex_guard = d.wait_mutex.lock().unwrap();
                // Communicate intent to wait to publisher
                let _dummy: bool = signal_needed.swap(true, AcqRel);
                // Verify that no slot was published
                pollable.poll();
                if min_available > pollable.len_available() {
                    // Sleep
                    let lock_result = d.wait_condvar.wait(mutex_guard);
                    assert!(lock_result.is_ok());
                    pollable.poll();
                }
            }
        }
    }

    fn notify_all_waiters(&mut self) {
        let d;
        unsafe {
            d = self.d.get_mut();
        }

        // Check if there are any waiters, resetting the value to false
        let signal_needed = d.signal_needed.swap(false, AcqRel);

        // If so, acquire the lock and signal on the wait condition
        if signal_needed {
            {
                let _mutex_guard = d.wait_mutex.lock().unwrap();
                d.wait_condvar.notify_all();
            }

            // This is a bit of a hack to work around the fact that the Mutex will occasionally
            // start executing the publisher's task when the consumer unlocks it, starving the
            // consumer. Doing this should cause the consumer to execute again, avoiding deadlock.
            // At the same time, it's mostly off the fast path (this code path is only hit if a long
            // gap in publishing caused one or more consumers to sleep), so performance shouldn't be
            // hurt much.
            thread::yield_now();
        }
    }
}

impl PollingWaitStrategy for BlockingWaitStrategy {
    fn wait_for_dependencies(&self, pollable: &mut dyn Pollable, min_available: usize) {
        let w = YieldWaitStrategy::new_with_retry_count(
            self.max_spin_tries_publisher,
            self.max_spin_tries_consumer,
        );

        w.wait_for_dependencies(pollable, min_available)
    }
}

impl fmt::Debug for BlockingWaitStrategy {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "BlockingWaitStrategy{{p: {}, c: {}}}",
            self.max_spin_tries_publisher, self.max_spin_tries_consumer
        )
    }
}

/// Defines methods related to waiting for slots to be available on a reference to a pipeline.
trait WaitForSlots: LenAvailable {
    /// Wait for N slots to be available.
    ///
    /// # Arguments
    ///
    /// * min_available - How many slots should be available before returning.
    ///
    /// # Safety notes
    ///
    /// Note that if N is greater than the size of the ring buffer minus the total number of slots the
    /// rest of the pipeline is waiting for, then this function may deadlock. A size of 1 should
    /// always be safe. Alternatively, increase the size of the buffer to support the desired amount
    /// of batching.
    fn wait_for_slots(&mut self, min_available: usize);

    /// Wait for a single slot to be available.
    fn wait_for_one_slot(&mut self) {
        self.wait_for_slots(1)
    }
}

/// Responsible for ensuring that the caller does not proceed until one or more dependent sequences
/// have finished working with the subsequent slots.
trait SequenceBarrier {
    type T;

    // Ring buffer related operations

    /// Stores a value in the sequence barrier's current slot.
    ///
    /// # Safety notes
    ///
    /// It's the caller's responsibility to avoid data races, so this function is unsafe. Races could
    /// occur in cases where multiple barriers are waiting on the same dependency and accessing slots
    /// in parallel.
    unsafe fn set(&mut self, value: Self::T);

    /// Gets the value stored in the sequence barrier's current slot, which would have been stored
    /// there by a different task, in most cases). Unsafe: allows data races.
    ///
    /// Mutable to facilitate transparent transitions to larger buffers.
    unsafe fn get(&mut self) -> &Self::T;
}

trait SequenceBarrierTake: SequenceBarrier {
    /// Takes the value stored in the sequence barrier's current slot, moving it out of the ring
    /// buffer. Unsafe: allows data races.
    unsafe fn take(&mut self) -> Self::T;
}

pub trait InsertSingleConsumer {
    type SingleConsumer;

    /// Adds a new pipeline stage, consisting of a single consumer that has exclusive access to the
    /// items it processes. The new consumer waits for elements to be released by whatever
    /// dependencies self was previously waiting on, while self now depends on the new consumer to
    /// release elements.
    fn insert_single_consumer(&mut self) -> Self::SingleConsumer;
}

/// A common definition of the fields that are shared between non-concurrent publisher or consumer
/// pipeline stages.
struct CommonSingleSequenceBarrier<RB, D> {
    ring_buffer: RB,
    sequence: SequenceOwner,
    dependencies: D,
    /// Contains the number of available items as of the last time the dependent sequence values were
    /// retrieved.
    cached_available: usize,
}

/// A common implementation of functions that can be shared between publisher and
/// consumer types.
impl<RB, D> CommonSingleSequenceBarrier<RB, D> {
    fn new(
        ring_buffer: RB,
        sequence: SequenceOwner,
        dependencies: D,
        cached_available: usize,
    ) -> Self {
        Self {
            ring_buffer,
            sequence,
            dependencies,
            cached_available,
        }
    }

    fn set_cached_available(&mut self, available: usize) {
        self.cached_available = available
    }
}

impl<RB, D> LenAvailable for CommonSingleSequenceBarrier<RB, D> {
    fn len_available(&self) -> usize {
        self.cached_available
    }
}

impl<RB, D> Pollable for CommonSingleSequenceBarrier<RB, D>
where
    D: PollableDependency,
    RB: UnsafeRingBufferOps,
{
    fn poll(&mut self) {
        self.cached_available = self
            .dependencies
            .calculate_available(self.sequence.get_owned(), self.capacity())
    }
}

impl<RB, D> PipelineCapacity for CommonSingleSequenceBarrier<RB, D>
where
    RB: UnsafeRingBufferOps,
{
    fn capacity(&self) -> usize {
        self.ring_buffer.len()
    }
}

impl<RB, D> ReleaseSlots for CommonSingleSequenceBarrier<RB, D>
where
    RB: UnsafeRingBufferOps,
{
    unsafe fn release_slots_unchecked(&mut self, n: usize) {
        debug_assert!(self.cached_available >= n);
        // SAFETY: the caller has promised this won't overflow.
        unsafe {
            self.cached_available = self.cached_available.unchecked_sub(n);
        }
        self.sequence.advance_and_flush(n, self.ring_buffer.len());
    }
}

impl<RB, D> CurrentSequence for CommonSingleSequenceBarrier<RB, D> {
    fn current_sequence(&self) -> SequenceNumber {
        self.sequence.get_owned()
    }
}

/// A common implementation of functions that can be shared between publisher and
/// consumer types, where the [`UnsafeRingBufferOps`] trait is required.
impl<RB, D> CommonSingleSequenceBarrier<RB, D>
where
    RB: UnsafeRingBufferOps,
{
    /// See [`SequenceBarrier::set`].
    unsafe fn set(&mut self, value: RB::T) {
        unsafe {
            let current_sequence = self.current_sequence();
            self.ring_buffer.set(current_sequence, value)
        }
    }

    /// See [`SequenceBarrier::get`].
    unsafe fn get(&mut self) -> &RB::T {
        unsafe {
            let current_sequence = self.current_sequence();
            self.ring_buffer.get(current_sequence)
        }
    }
}

/// A common implementation of functions that can be shared between publisher and
/// consumer types, where the [`UnsafeRingBufferOpsTake`] trait is required.
impl<RB, D> CommonSingleSequenceBarrier<RB, D>
where
    RB: UnsafeRingBufferOpsTake,
{
    /// See [`SequenceBarrierTake::take`].
    unsafe fn take(&mut self) -> RB::T {
        unsafe {
            let current_sequence = self.current_sequence();
            self.ring_buffer.take(current_sequence)
        }
    }
}

/// Implements `SequenceBarrier` for publishers in situations where there's only one concurrent
/// publisher.
struct SinglePublisherSequenceBarrier<RB, W> {
    sb: CommonSingleSequenceBarrier<RB, PublisherDependencies>,
    wait_strategy: W,
}

impl<RB, W> SinglePublisherSequenceBarrier<RB, W> {
    fn new(ring_buffer: RB, wait_strategy: W) -> Self {
        Self {
            sb: CommonSingleSequenceBarrier::new(
                ring_buffer,
                SequenceOwner::new(),
                PublisherDependencies::default(),
                0,
            ),
            wait_strategy,
        }
    }
}

impl<RB, W> SinglePublisherSequenceBarrier<RB, W>
where
    RB: UnsafeRingBufferOps,
{
    /// Returns the earliest sequence that may not yet be handled by the rest of the pipeline.
    ///
    /// This is used in the insertion of new consumers as their initial sequence number.
    fn calculate_earliest_consumer_sequence(&mut self) -> SequenceNumber {
        if self.sb.dependencies.sequences.is_empty() {
            SequenceNumber(SEQUENCE_INITIAL)
        } else {
            // Poll dependencies once, to ensure the latest possible information is used in this
            // calculation.
            self.sb.poll();

            let slots_in_progress = self.sb.capacity() - self.sb.cached_available;
            self.sb
                .sequence
                .get_owned()
                .wrapping_sub(slots_in_progress, wrap_boundary(self.sb.capacity()))
        }
    }
}

impl<RB, W> AsPipelineRef for SinglePublisherSequenceBarrier<RB, W>
where
    RB: UnsafeRingBufferOps,
{
    type T = CommonSingleSequenceBarrier<RB, PublisherDependencies>;

    fn as_pipeline_ref(&self) -> &Self::T {
        &self.sb
    }
    fn as_pipeline_ref_mut(&mut self) -> &mut Self::T {
        &mut self.sb
    }
}

impl<RB, W> WaitForSlots for SinglePublisherSequenceBarrier<RB, W>
where
    RB: UnsafeRingBufferOps,
    W: NotificationWaitStrategy,
{
    fn wait_for_slots(&mut self, min_available: usize) {
        self.wait_strategy
            .wait_for_dependencies(&mut self.sb, min_available);
    }
}

impl<RB, W> ReleaseSlots for SinglePublisherSequenceBarrier<RB, W>
where
    RB: UnsafeRingBufferOps,
    W: NotificationWaitStrategy,
{
    unsafe fn release_slots_unchecked(&mut self, n: usize) {
        // SAFETY: delegated to the caller.
        unsafe { self.sb.release_slots_unchecked(n) };
        self.wait_strategy.notify_all_waiters();
    }
}

impl<RB, W> SequenceBarrier for SinglePublisherSequenceBarrier<RB, W>
where
    RB: UnsafeRingBufferOps,
    W: NotificationWaitStrategy,
{
    type T = RB::T;

    unsafe fn set(&mut self, value: RB::T) {
        unsafe { self.sb.set(value) }
    }

    unsafe fn get(&mut self) -> &RB::T {
        unsafe { self.sb.get() }
    }
}

impl<RB, W> SequenceBarrierTake for SinglePublisherSequenceBarrier<RB, W>
where
    W: NotificationWaitStrategy,
    RB: UnsafeRingBufferOpsTake,
{
    unsafe fn take(&mut self) -> RB::T {
        unsafe {
            let current_sequence = self.current_sequence();
            self.sb.ring_buffer.take(current_sequence)
        }
    }
}

impl<RB, W> InsertSingleConsumer for SinglePublisherSequenceBarrier<RB, W>
where
    W: Clone,
    RB: UnsafeRingBufferOps + Clone,
{
    type SingleConsumer = SingleConsumerSequenceBarrier<RB, W>;

    /// Insert a new consumer.
    ///
    /// # Behaviour while racing with existing consumers
    ///
    /// While most users will want to call this before emitting any events into the pipeline, it's
    /// safe to call after passing the consumer handles to other threads, even while events are
    /// partially handled by the rest of the pipeline. However, the behaviour is non-deterministic,
    /// for now.
    ///
    /// Intuitively, the least surprising behaviour would arguably be for the newly inserted
    /// consumer to only handle events that are inserted after the consumer was inserted. However,
    /// that's not supported by the current availability calculation algorithm.
    ///
    /// Instead, the implementation settles for the next best thing, by reducing the sequence just
    /// enough to guarantee that it'll be less than or equal to the sequence(s) of the previous
    /// stage in the pipeline. This means that the newly inserted consumer will process any items
    /// that were previously inserted into the pipeline, but not yet observed by the publisher to
    /// be fully handled as of when the new consumer is inserted.
    ///
    /// It's possible to block until the pipeline is flushed, if there's a use case that would
    /// benefit from that, or it may be possible to make the availability calculation a bit more
    /// complicated in order to make this work.
    fn insert_single_consumer(&mut self) -> Self::SingleConsumer {
        let new_sequence = self.calculate_earliest_consumer_sequence();

        let my_dependencies = &mut self.sb.dependencies;
        let sequences = if my_dependencies.sequences.is_empty() {
            // This is the first addition to the pipeline, so use this publisher's sequence as its
            // dependency.
            vec![self.sb.sequence.clone_immut()]
        } else {
            // A newly inserted consumer needs to wait for what was previously the last stage in the
            // pipeline, so give the new consumer the existing dependency list.
            std::mem::take(&mut my_dependencies.sequences)
        };
        let dependencies = ConsumerDependencies::from_vec(sequences);
        // my_dependencies is an empty Vec now, to be populated with a reference to the new
        // consumer's sequence.

        let new_consumer = SingleConsumerSequenceBarrier::new(
            self.sb.ring_buffer.clone(),
            new_sequence,
            dependencies,
            0,
            self.wait_strategy.clone(),
            // Our sequence is the publisher's sequence (aka the cursor)
            self.sb.sequence.clone_immut(),
        );

        my_dependencies.sequences.reserve_exact(1);
        my_dependencies
            .sequences
            .push(new_consumer.sb.sequence.clone_immut());

        new_consumer
    }
}

/// Implements `SequenceBarrier` for consumers. This implementation supports multiple concurrent
/// consumers, but all consumers will process all events. This is unsuitable for when a
/// load-balancing arrangement is desired.
struct SingleConsumerSequenceBarrier<RB, W> {
    sb: CommonSingleSequenceBarrier<RB, ConsumerDependencies>,
    /// A reference to the publisher's sequence.
    publisher_availability: PublisherAvailability,
    wait_strategy: W,
}

impl<RB, W> SingleConsumerSequenceBarrier<RB, W> {
    fn new(
        ring_buffer: RB,
        initial_sequence: SequenceNumber,
        dependencies: ConsumerDependencies,
        cached_available: usize,
        wait_strategy: W,
        publisher_sequence: SequenceReader,
    ) -> SingleConsumerSequenceBarrier<RB, W> {
        SingleConsumerSequenceBarrier {
            sb: CommonSingleSequenceBarrier::new(
                ring_buffer,
                SequenceOwner::new_from_sequence(initial_sequence),
                dependencies,
                cached_available,
            ),
            publisher_availability: PublisherAvailability {
                sequence: publisher_sequence,
            },
            wait_strategy,
        }
    }
}

impl<RB, W> AsPipelineRef for SingleConsumerSequenceBarrier<RB, W>
where
    RB: UnsafeRingBufferOps,
{
    type T = CommonSingleSequenceBarrier<RB, ConsumerDependencies>;

    fn as_pipeline_ref(&self) -> &Self::T {
        &self.sb
    }
    fn as_pipeline_ref_mut(&mut self) -> &mut Self::T {
        &mut self.sb
    }
}

impl<RB, W> WaitForSlots for SingleConsumerSequenceBarrier<RB, W>
where
    W: NotificationWaitStrategy,
    RB: UnsafeRingBufferOps,
{
    fn wait_for_slots(&mut self, min_available: usize) {
        let current_sequence = self.current_sequence();
        self.wait_strategy.wait_for_publisher(
            &mut self
                .publisher_availability
                .as_pollable(current_sequence, self.sb.ring_buffer.len()),
            min_available,
        );
        self.wait_strategy
            .wait_for_dependencies(&mut self.sb, min_available);
    }
}

impl<RB, W> DelegateReleaseSlots for SingleConsumerSequenceBarrier<RB, W> {}

impl<RB, W> SequenceBarrier for SingleConsumerSequenceBarrier<RB, W>
where
    W: NotificationWaitStrategy,
    RB: UnsafeRingBufferOps,
{
    type T = RB::T;

    unsafe fn set(&mut self, value: Self::T) {
        unsafe { self.sb.set(value) }
    }
    unsafe fn get(&mut self) -> &Self::T {
        unsafe { self.sb.get() }
    }
}

impl<RB, W> SequenceBarrierTake for SingleConsumerSequenceBarrier<RB, W>
where
    RB: UnsafeRingBufferOpsTake,
    W: NotificationWaitStrategy,
{
    unsafe fn take(&mut self) -> Self::T {
        unsafe { self.sb.take() }
    }
}

impl<RB, W> InsertSingleConsumer for SingleConsumerSequenceBarrier<RB, W>
where
    W: Clone,
    RB: Clone,
{
    type SingleConsumer = Self;

    fn insert_single_consumer(&mut self) -> Self {
        // Reuse self's dependencies, and populate its replacement below, after constructing the new
        // consumer.
        let new_dependencies = std::mem::take(&mut self.sb.dependencies);
        let new_consumer = Self::new(
            self.sb.ring_buffer.clone(),
            self.sb.sequence.get_owned(),
            new_dependencies,
            self.sb.cached_available,
            self.wait_strategy.clone(),
            self.publisher_availability.sequence.clone(),
        );

        // Wait for the new consumer to process the events that would otherwise have been available
        // to self.
        self.sb.dependencies.sequences.reserve_exact(1);
        self.sb
            .dependencies
            .sequences
            .push(new_consumer.sb.sequence.clone_immut());
        self.sb.cached_available = 0;
        new_consumer
    }
}

/// Allows callers to send items through a disruptor pipeline.
pub trait Publisher<T> {
    /// Sends a single item into the pipeline. The value will be exposed to each consumer downstream
    /// in the pipeline.
    fn publish(&self, value: T);
}

/// Provides access to values that are passing through the pipeline.
pub trait Consumer<T> {
    /// Waits for a single item to become available, then calls the given function to process the
    /// value.
    fn consume<C>(&self, consume_callback: C)
    where
        C: FnMut(&T);
}

/// Consumers that aren't sharing a pipeline stage with other consumers can mutate items.
pub trait ConsumerMut<T>: Consumer<T> {
    /// Waits for the next value to be available, moves it out of the ring buffer, and returns it.
    fn take(&self) -> T;
}

/// Allows callers to wire up dependencies, then send values down the pipeline
/// of dependent consumers.
struct GenericPublisher<SB> {
    sequence_barrier: UnsafeCell<SB>,
}

impl<SB> GenericPublisher<SB> {
    /// Generic constructor that works with any UnsafeRingBufferOps implementation
    fn new_common(sb: SB) -> GenericPublisher<SB> {
        GenericPublisher {
            sequence_barrier: UnsafeCell::new(sb),
        }
    }
}

impl<SB> GenericPublisher<SB>
where
    SB: WaitForSlots + ReleaseSlots + SequenceBarrier,
{
    // In the worst case (minimal microbenchmarking), call overhead is significant.
    #[inline]
    fn publish(&self, value: SB::T) {
        // SAFETY:
        // 1. &Self isn't Send, so access will be single-threaded, in any case.
        // 2. The reference only exists in the scope of this function, maintaining aliasing rules.
        let sb = unsafe { &mut *self.sequence_barrier.get() };
        // Wait for available slot
        sb.wait_for_one_slot();
        // SAFETY: calling wait_for_one_slot synchronizes with other threads, ensuring this write
        // won't create a data race.
        unsafe {
            sb.set(value);
        }
        // Make the item available to downstream consumers
        // SAFETY: the above call to wait_for_one ensures this is safe.
        unsafe { sb.release_slots_unchecked(1) };
    }
}

impl<SB> InsertSingleConsumer for GenericPublisher<SB>
where
    SB: InsertSingleConsumer,
{
    type SingleConsumer = GenericSingleConsumer<SB::SingleConsumer>;

    fn insert_single_consumer(&mut self) -> Self::SingleConsumer {
        GenericSingleConsumer::new(GenericSharedConsumer::new(
            self.sequence_barrier.get_mut().insert_single_consumer(),
        ))
    }
}

/// Allows callers to retrieve values from upstream tasks in the pipeline.
struct GenericSharedConsumer<SB> {
    sequence_barrier: UnsafeCell<SB>,
}

impl<SB> GenericSharedConsumer<SB> {
    fn new(sb: SB) -> GenericSharedConsumer<SB> {
        GenericSharedConsumer {
            sequence_barrier: UnsafeCell::new(sb),
        }
    }
}

impl<SB> GenericSharedConsumer<SB>
where
    SB: WaitForSlots + ReleaseSlots + SequenceBarrier,
{
    fn consume<C: FnMut(&SB::T)>(&self, mut consume_callback: C) {
        // SAFETY:
        // 1. &Self isn't Send, so access will be single-threaded, in any case.
        // 2. The reference only exists in the scope of this function, maintaining aliasing rules.
        let sequence_barrier = unsafe { &mut *self.sequence_barrier.get() };

        sequence_barrier.wait_for_one_slot();
        // SAFETY: calling wait_for_one_slot synchronizes with other threads, ensuring this read
        // won't create a data race.
        let item = unsafe { sequence_barrier.get() };
        consume_callback(item);
        // SAFETY: the above call to wait_for_one ensures this is safe.
        unsafe { sequence_barrier.release_slots_unchecked(1) };
    }
}

#[cfg(test)]
mod generic_publisher_tests {
    use crate::{
        Consumer, ConsumerMut, InsertSingleConsumer, Publisher, SinglePublisher, SpinWaitStrategy,
        wrap_boundary,
    };

    #[test_log::test]
    fn send_single_value() {
        let mut publisher = SinglePublisher::<isize, 1, SpinWaitStrategy>::new(SpinWaitStrategy);
        let consumer = publisher.insert_single_consumer();
        publisher.publish(1);
        consumer.consume(|value: &isize| {
            assert!(*value == 1);
        });
    }
    #[test_log::test]
    fn send_single_value_via_take() {
        let mut publisher = SinglePublisher::<isize, 1, SpinWaitStrategy>::new(SpinWaitStrategy);
        let consumer = publisher.insert_single_consumer();
        let value = 1;
        publisher.publish(value);
        let received_value = consumer.take();
        assert_eq!(received_value, value);
    }

    #[test_log::test]
    fn test_sequence_wrapping() {
        const CAPACITY: usize = 8;
        let mut publisher =
            SinglePublisher::<isize, CAPACITY, SpinWaitStrategy>::new(SpinWaitStrategy);
        let mut next_published_item = 1;
        let consumer = publisher.insert_single_consumer();
        let mut next_consumed_item = 1;

        // Fill the buffer
        for _ in 0..(CAPACITY as isize) {
            publisher.publish(next_published_item);
            next_published_item += 1;
        }

        // Increase the sequences, one at a time, until they both wrap.
        for _ in 0..(wrap_boundary(CAPACITY)) {
            assert!(next_consumed_item == consumer.take());
            next_consumed_item += 1;
            publisher.publish(next_published_item);
            next_published_item += 1;
        }
    }

    // TODO: test that dependencies hold true by setting up a chain, grabbing a list of timestamps
    // within each task, and then verifying them after for a happens-before relationship. It's not
    // foolproof, but better than nothing.
}

/// A consumer in the pipeline that doesn't share its stage with any concurrent consumers, allowing
/// it to have mutable access to the elements it processes.
struct GenericSingleConsumer<SB> {
    sc: GenericSharedConsumer<SB>,
}

impl<SB> GenericSingleConsumer<SB> {
    /// Return a new instance wrapped around a given GenericSharedConsumer instance. In addition to
    /// existing features, it also allows the caller to take ownership of the items it accesses.
    fn new(sc: GenericSharedConsumer<SB>) -> GenericSingleConsumer<SB> {
        GenericSingleConsumer { sc }
    }
}

impl<SB> InsertSingleConsumer for GenericSingleConsumer<SB>
where
    SB: InsertSingleConsumer<SingleConsumer = SB>,
{
    type SingleConsumer = Self;

    fn insert_single_consumer(&mut self) -> Self::SingleConsumer {
        Self {
            sc: GenericSharedConsumer::new(
                self.sc.sequence_barrier.get_mut().insert_single_consumer(),
            ),
        }
    }
}

impl<SB> GenericSingleConsumer<SB>
where
    SB: WaitForSlots + ReleaseSlots + SequenceBarrier,
{
    /// See [`GenericSharedConsumer::consume`].
    fn consume<C: FnMut(&SB::T)>(&self, consume_callback: C) {
        self.sc.consume(consume_callback)
    }
}

impl<SB> GenericSingleConsumer<SB>
where
    SB: WaitForSlots + ReleaseSlots + SequenceBarrierTake,
{
    fn take(&self) -> SB::T {
        // SAFETY:
        // 1. &Self isn't Send, so access will be single-threaded, in any case.
        // 2. The reference only exists in the scope of this function, maintaining aliasing rules.
        let sequence_barrier = unsafe { &mut *self.sc.sequence_barrier.get() };
        sequence_barrier.wait_for_one_slot();
        // SAFETY: calling wait_for_one_slot synchronizes with other threads, ensuring this read
        // won't create a data race.
        let value = unsafe { sequence_barrier.take() };
        // SAFETY: the above call to wait_for_one ensures this is safe.
        unsafe { sequence_barrier.release_slots_unchecked(1) };
        value
    }
}

/// Used to implement in-band signalling about buffer reallocations. Will nearly always contain an
/// [`Item`](Self::Item), but during buffer reallocation, the last slot in the old buffer will contain
/// [`BufferReallocated`](Self::BufferReallocated) instead.
#[derive(Debug, Copy, Clone)]
enum ReallocationFlag<T> {
    BufferReallocated,
    Item(T),
}

impl<T> ReallocationFlag<T> {
    /// Returns `true` if this flag contains an [`Item`](Self::Item), like [`Option::is_some`].
    fn is_item(&self) -> bool {
        matches!(self, Self::Item(_))
    }

    /// Converts from `&ReallocationFlag<T>` to `ReallocationFlag<&T>`, like [`Option::as_ref`].
    fn as_ref(&self) -> ReallocationFlag<&T> {
        match *self {
            Self::BufferReallocated => ReallocationFlag::BufferReallocated,
            Self::Item(ref x) => ReallocationFlag::Item(x),
        }
    }

    /// Similar to [`Option::unwrap`].
    fn unwrap(self) -> T {
        match self {
            Self::BufferReallocated => {
                panic!("Called ReallocationFlag::unwrap on a `BufferReallocated` value")
            }
            Self::Item(x) => x,
        }
    }
}

impl<T> Default for ReallocationFlag<T>
where
    T: Default,
{
    /// Returns an [`Item(T)`](Self::Item) containing `T`'s default value.
    fn default() -> Self {
        Self::Item(T::default())
    }
}

/// Now, implement a resizable version of the disruptor. After waiting sufficiently long enough for
/// the consumer pipeline to release slots, the publisher will instead allocate a new, larger, ring
/// buffer, write a special value to the corresponding slot in the old ring buffer, and store the
/// actual value in the new ring buffer. A pointer from the old buffer to the new one is also
/// written. The publisher then makes these changes visible to the downstream pipeline by
/// incrementing its sequence value. If the value has wrapped recently, the publisher bumps it back
/// above the consumers' sequence values, to avoid ambiguity resulting from the larger buffer size.
/// Finally, the last consumer in the pipeline deallocates the old buffer before moving on to the
/// larger buffer. Currently, the lifetime of old buffers is managed via reference counting.
///
/// NOTE: Latency sensitive applications should not use this mode: the unbounded queue buildup will
/// increase latency outside of acceptable levels. Instead, they should gracefully handle excess
/// demand by providing whatever feedback is needed to reduce upstream demand to levels that the
/// application can handle. For example, this may involve dropping packets, queueing users that try
/// to open new sessions (to avoid degrading service for existing sessions), skipping frames, or any
/// other mechanism that reduces demand as early as possible to avoid wasted effort.
///
/// # Availability calculation following reallocation
///
/// Although it's not strictly necessary, things are most efficient if the publisher immediately uses
/// all of the slots in the newly allocated buffer without waiting for the consumer. After resizing
/// the buffer, the publisher will start using the increased buffer size in availability
/// calculations. This allows the publisher to use (new_size - old_size) extra slots, but it leaves
/// old_size slots unused.
///
/// To fix this, the publisher manipulates its cached availability value to reflect the extra
/// available slots. This avoids the need to add extra code and state just to solve this problem.
///
/// # Example
///
/// Imagine a ring buffer of size 4. All stages in the pipeline start with a sequence of 0. The
/// publisher publishes 15 items, which leaves its sequence number at 15. For this to be possible,
/// the consumer(s) must have processed some of the items, since the buffer size, 4, is less than 15.
/// Actually, only 3 elements are available in the buffer, the 4th is reserved as a way for the
/// publisher to communicate that it has allocated another buffer. The publisher then publishes its
/// 16th item into the slot corresponding to sequence 15. When it increments its sequence number, it
/// wraps back down to 0, because the wrap boundary is 16 (four times the buffer size).
///
/// Now, let's imagine the application is somehow written to contain a deadlock. For example, the
/// consumer is receiving from the publisher via two different communication channels, and it waits
/// for the publisher to send something on the second, while the publisher is waiting for the
/// consumer to process items from the first. When the resizing support is in use, the publisher will
/// eventually decide to reallocate a larger buffer of size 8.
///
/// During the reallocation, the final consumer's sequence number is at least 13
/// (`16 - (buffer_size - 1)`), since the publisher has been taking care to leave an extra slot free
/// in case a reallocation is needed. The consumer's sequence will never exceed the publisher's,
/// except through wrapping, so we can also say that it is logically at most 16, where 16 would be
/// wrapped to 0. Therefore, the consumer's sequence value could be any of {13, 14, 15, 0}.  The
/// publisher's sequence value is 0 until reallocation is complete, at which point it will be
/// unwrapped back to 16, and incremented to 17. One important thing to note is that regardless of
/// whether the publisher's sequence value was 0, 4, 8, or 12, it will be unwrapped to 16.
///
/// If the consumer's sequence value was 13 prior to the reallocation, then there were 0 slots
/// available for the publisher to use: with a buffer size of 4, the last slot available to the
/// publisher would have been at sequence value 16, minus the one slot that was reserved for
/// signalling reallocations. In other words, 15 was the last available slot, and the publisher has
/// already used it.
///
/// After the reallocation, though, the availability calculation would be using the larger buffer
/// size of 8, and as a result would conclude that the last available slot is 20 (subtracting one
/// leaves 19), so the publisher is now able to publish 4 more times into slots 16-19. However, there
/// are actually 7 more slots available (8 minus the one reserved slot). To use the extra slots, the
/// publisher modifies its cached availability value to indicate that 7 slots are available.  This
/// allows it to immediately publish into slots 20, 21, and 22 as well, leaving its sequence value at
/// 23 instead of 20.
///
/// This results in several new special cases to handle versus the usual non-resizing variant:
/// - From the consumer's perspective, there may be more than `buffer_size` slots available, because
///   the publisher has started publishing into the new buffer.
/// - When calculating availability from the publisher's perspective, the consumer sequence that
///   gates its publishing may be more than `buffer_size` slots behind. The availability calculation
///   needs to return 0 in this case.
/// - It becomes important to ensure that the publisher does not wrap until the consumer pipeline has
///   transitioned to the new buffer, to avoid breaking the consumer's availability calculations.
///
/// The publisher's availability calculation function was rewritten to correctly handle the second
/// point, and the wrap boundary was changed to `4*buffer_size` to facilitate the third point.
struct ResizableRingBufferData<T> {
    rb_data: BoxedRingBufferData<T>,
    /// When non-null, points to a larger buffer allocated by the publisher to replace this one.
    next: Option<UncheckedUnsafeArc<ResizableRingBufferData<T>>>,
}

impl<T> ResizableRingBufferData<T>
where
    T: Default,
{
    /// Constructs a new ring buffer with the given size.
    fn new(size: usize) -> ResizableRingBufferData<T> {
        ResizableRingBufferData {
            rb_data: BoxedRingBufferData::new(size),
            next: None,
        }
    }

    /// Reallocates a larger ring buffer, and stores a pointer to the new buffer in `next`.  A
    /// reference to the new buffer is returned.
    ///
    /// # Invariants
    ///
    /// This expects to be called only once for a given instance.
    ///
    /// # Safety
    ///
    /// Calling this concurrently from multiple threads would result in a data race (in addition to
    /// violating the expectation that this be called once.
    unsafe fn reallocate(
        &mut self,
        new_size: usize,
    ) -> UncheckedUnsafeArc<ResizableRingBufferData<T>> {
        let new_rrbd = ResizableRingBufferData::new(new_size);
        debug_assert!(self.next.is_none(), "reallocate called multiple times");
        self.next = Some(UncheckedUnsafeArc::new(new_rrbd));
        self.next.as_mut().unwrap().clone()
    }
}

impl<T> RingBufferAsSlice for ResizableRingBufferData<T> {
    type Element = CachePadded<T>;
    type T = T;

    fn as_slice(&self) -> &[Self::Element] {
        self.rb_data.as_slice()
    }

    fn as_mut_slice(&mut self) -> &mut [Self::Element] {
        self.rb_data.as_mut_slice()
    }
}

/// Like [`RingBufferArc`], but can also allow publishers to reallocate a larger buffer and expose
/// it to consumers. The consumers retrieve the remaining items from the old buffer until they reach
/// a flagged element left by the publisher, which signals to them that they should traverse the
/// pointer and retrieve items from the next buffer from now on.
struct ResizableRingBufferArc<T> {
    d: UncheckedUnsafeArc<ResizableRingBufferData<T>>,
}

/// Enables the use of a blanket UnsafeRingBufferDeref implementation.
impl<T> Deref for ResizableRingBufferArc<T> {
    type Target = UncheckedUnsafeArc<ResizableRingBufferData<T>>;

    fn deref(&self) -> &Self::Target {
        &self.d
    }
}

/// Enables the use of a blanket UnsafeRingBufferDeref implementation.
impl<T> DerefMut for ResizableRingBufferArc<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.d
    }
}

impl<T> ResizableRingBufferArc<T>
where
    T: Default,
{
    /// Construct a new [`ResizableRingBufferData`] with a capacity for `size` elements. As with
    /// [`BoxedRingBufferData`], `size` must be a power of two.
    fn new(size: usize) -> ResizableRingBufferArc<T> {
        ResizableRingBufferArc {
            d: UncheckedUnsafeArc::new(ResizableRingBufferData::new(size)),
        }
    }

    /// Allocates a new ring buffer of the given size, replaces self with a reference to the
    /// newly allocated buffer, and returns the old reference.
    ///
    /// # Safety
    ///
    /// Calling this concurrently from multiple threads would result in a data race.
    unsafe fn reallocate(&mut self, new_size: usize) -> Self {
        // SAFETY: delegated to the caller.
        let new_rrbd = unsafe { self.d.get_mut().reallocate(new_size) };
        let old_arc = std::mem::replace(&mut self.d, new_rrbd);
        Self { d: old_arc }
    }
}

impl<T> ResizableRingBufferArc<T> {
    /// Returns a ResizableRingBufferArc pointing to the next ResizableRingBufferData.
    ///
    /// # Panics
    ///
    /// If called before a reallocation has occurred.
    ///
    /// # Safety
    ///
    /// This should only be called after a reallocation has occurred. The caller must establish a
    /// happens-before relationship between the reallocation and a call to this function, otherwise
    /// calling this can result in a data race.
    unsafe fn get_next(&mut self) -> Self {
        // SAFETY: based on the invariants delegated to the caller of this function, as well as the
        // caller to reallocate, access to the `next` field below cannot race with a concurrent
        // write.
        let rrbd = unsafe { self.d.get_mut() };
        let next_rrbd = rrbd.next.as_ref().unwrap().clone();
        Self { d: next_rrbd }
    }
}

impl<T> Clone for ResizableRingBufferArc<T> {
    /// Copy a reference to the original buffer.
    fn clone(&self) -> ResizableRingBufferArc<T> {
        ResizableRingBufferArc { d: self.d.clone() }
    }
}

#[cfg(test)]
#[test_log::test]
fn test_resizeable_ring_buffer() {
    // General smoke test
    let mut publisher_rb = ResizableRingBufferArc::<ReallocationFlag<usize>>::new(2);
    let mut consumer_rb = publisher_rb.clone();

    // Dummy values
    let v = [123, 231, 312];

    // Hypothetical publisher writes a value, then resizes, then writes 2 more values (3 total,
    // whereas the initial buffer only holds two values)
    let mut s = 0;

    // SAFETY: this test is single-threaded.
    unsafe {
        publisher_rb.set(SequenceNumber(s), ReallocationFlag::Item(v[0]));
    }

    s += 1;

    let mut old_rb = unsafe { publisher_rb.reallocate(4) };
    unsafe {
        old_rb.set(SequenceNumber(s), ReallocationFlag::BufferReallocated);
    }
    unsafe {
        publisher_rb.set(SequenceNumber(s), ReallocationFlag::Item(v[1]));
    }
    s += 1;
    unsafe {
        publisher_rb.set(SequenceNumber(s), ReallocationFlag::Item(v[2]));
    }

    // The consumer gets all three values, switching to the next buffer as needed
    for (s2, i) in v.iter().enumerate() {
        let mut flag = unsafe { consumer_rb.take(SequenceNumber(s2)) };
        let mut switch_occurred = false;
        if !flag.is_item() {
            consumer_rb = unsafe { consumer_rb.get_next() };
            switch_occurred = true;
            flag = unsafe { consumer_rb.take(SequenceNumber(s2)) };
        }
        let value = flag.unwrap();
        if s2 == 1 {
            assert!(switch_occurred);
        }
        assert_eq!(value, *i);
    }
}

/// Returns how many slots are open between the publisher's sequence and the consumer's sequence,
/// taking into account the effects of wrapping, and also resizing.
fn calculate_available_publisher_resizing(
    gating_sequence: SequenceNumber,
    waiting_sequence: SequenceNumber,
    buffer_size: usize,
) -> usize {
    let SequenceNumber(gating_value) = gating_sequence;
    let SequenceNumber(mut waiting_value) = waiting_sequence;
    // Handle wrapping
    if waiting_value < gating_value {
        waiting_value += wrap_boundary(buffer_size);
    }
    let first_unavailable_slot = gating_value + buffer_size;
    // Handle resizing
    if first_unavailable_slot < waiting_value {
        return 0;
    }
    let available = first_unavailable_slot - waiting_value;
    available
}

#[cfg(test)]
#[test_log::test]
fn test_calculate_available_publisher_resizing() {
    // Test a few in sequence:
    let test =
        |g, w, s| calculate_available_publisher_resizing(SequenceNumber(g), SequenceNumber(w), s);
    let buffer_size = 4;
    assert_eq!(test(0, 0, buffer_size), 4);
    assert_eq!(test(0, 1, buffer_size), 3);
    assert_eq!(test(0, 2, buffer_size), 2);
    // The publisher stops before using up last slot, last slot is reserved for signal
    assert_eq!(test(0, 3, buffer_size), 1);
    // The consumer catches up
    assert_eq!(test(3, 3, buffer_size), 4);
    // The publisher resizes here, new sequence value is 19 ( (6 % buffer_size) +
    // wrap_boundary(buffer_size) + 1).
    assert_eq!(test(3, 6, buffer_size), 1);
    let new_buffer_size = 8;
    assert_eq!(test(3, 19, new_buffer_size), 0);
}

/// A resizing-flavoured version of [`PublisherDependencies`].
#[derive(Clone, Debug, Default)]
struct ResizingPublisherDependencies {
    sequences: Vec<SequenceReader>,
}

impl PollableDependency for ResizingPublisherDependencies {
    fn calculate_available(&self, waiting_sequence: SequenceNumber, buffer_size: usize) -> usize {
        calculate_available_list(
            waiting_sequence,
            self.sequences.as_slice(),
            buffer_size,
            &calculate_available_publisher_resizing,
        )
    }
}

/// Resizing variant of SinglePublisherSequenceBarrier.
struct SingleResizingPublisherSequenceBarrier<T> {
    // Reuse SinglePublisherSequenceBarrier data declarations and constructor
    sb: CommonSingleSequenceBarrier<
        ResizableRingBufferArc<ReallocationFlag<T>>,
        ResizingPublisherDependencies,
    >,
    wait_strategy: TimeoutResizeWaitStrategy,
}

impl<T> SingleResizingPublisherSequenceBarrier<T> {
    fn new(
        ring_buffer: ResizableRingBufferArc<ReallocationFlag<T>>,
        wait_strategy: TimeoutResizeWaitStrategy,
    ) -> SingleResizingPublisherSequenceBarrier<T> {
        SingleResizingPublisherSequenceBarrier {
            sb: CommonSingleSequenceBarrier::new(
                ring_buffer,
                SequenceOwner::new(),
                ResizingPublisherDependencies::default(),
                0,
            ),
            wait_strategy,
        }
    }
}

impl<T> LenAvailable for SingleResizingPublisherSequenceBarrier<T> {
    fn len_available(&self) -> usize {
        // Don't expose the reserved slot. This ensures the default wait_for_slots implementation
        // will wait correctly in order to maintain the extra slot.
        self.sb.len_available().saturating_sub(1)
    }
}

impl<T> Pollable for SingleResizingPublisherSequenceBarrier<T>
where
    T: 'static,
{
    fn poll(&mut self) {
        // If `cached_available` was manually set during a reallocation, calls to this function
        // shouldn't overwrite it with a lower value.
        self.sb.cached_available = cmp::max(
            self.sb.cached_available,
            self.sb
                .dependencies
                .calculate_available(self.sb.sequence.get_owned(), self.sb.capacity()),
        )
    }
}

impl<T> CurrentSequence for SingleResizingPublisherSequenceBarrier<T> {
    fn current_sequence(&self) -> SequenceNumber {
        self.sb.current_sequence()
    }
}

impl<T> PipelineCapacity for SingleResizingPublisherSequenceBarrier<T>
where
    T: 'static,
{
    fn capacity(&self) -> usize {
        self.sb.capacity()
    }
}

impl<T> SequenceBarrier for SingleResizingPublisherSequenceBarrier<T>
where
    T: Default + 'static,
{
    type T = T;

    // Inherited functions
    unsafe fn set(&mut self, value: T) {
        unsafe { self.sb.set(ReallocationFlag::Item(value)) }
    }
    unsafe fn get(&mut self) -> &T {
        // Satisfy the borrow checker by performing this call outside the flag variable's lifetime.
        let current_sequence = self.sb.current_sequence();
        let flag = unsafe { self.sb.get() };
        debug_assert!(
            flag.is_item(),
            "Attempted borrow of `ReallocationFlag::BufferReallocated` at sequence: {}",
            current_sequence.value()
        );
        flag.as_ref().unwrap()
    }
}

impl<T> WaitForSlots for SingleResizingPublisherSequenceBarrier<T>
where
    T: Default + 'static,
{
    /// Wait for N slots to be available, or reallocate a larger buffer to hold it, if the resizing
    /// policy requests that.
    fn wait_for_slots(&mut self, min_available: usize) {
        let current_size = self.sb.capacity();

        // This uses the CommonSingleSequenceBarrier implementations of poll and len_available
        // (pending future refactoring), so adjust min_available accordingly.
        self.wait_strategy
            .try_wait_for_consumers(&mut self.sb, min_available + 1);

        if self.len_available() < min_available {
            // The wait strategy timed out, so allocate a new buffer here.

            // Make the new buffer twice as large
            let new_size = 2 * current_size;

            // If the sequence has been wrapped, then it is temporarily going to be less than
            // consumer sequence numbers in the other stages of the pipeline. If we allocate a
            // larger buffer and publish into it, then the publisher's sequence can overtake the
            // other sequences in the pipeline, which would break the availability calculations.
            // Unwrapping the publisher's sequence ensures that availability calculations remain
            // correct throughout the reallocation transition.
            let old_sequence = self.current_sequence();
            self.sb.sequence.unwrap(current_size);
            let unwrapped_sequence = self.current_sequence();

            // Resizing shouldn't be a normal part of a program's operation. Alert the user, so that
            // they can consider fixing the issue.
            error!(
                "Possible deadlock detected, allocating a larger buffer for disruptor events. Current buffer size: {}, new size: {}, batch size: {}",
                current_size, new_size, min_available
            );
            debug!(
                "sequence: {}, unwrapped sequence: {}",
                old_sequence.value(),
                unwrapped_sequence.value()
            );

            unsafe {
                // Signal to consumers that there's a larger buffer to transition to.
                self.sb.set(ReallocationFlag::BufferReallocated);
                self.sb.ring_buffer.reallocate(new_size);
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
            self.sb.set_cached_available(new_size);
        }
    }
}

impl<T> ReleaseSlots for SingleResizingPublisherSequenceBarrier<T>
where
    T: 'static,
{
    unsafe fn release_slots_unchecked(&mut self, n: usize) {
        // Assert this type's modified len_available implementation is also being respected.
        debug_assert!(self.len_available() >= n);

        // SAFETY: the caller promises this is safe.
        unsafe { self.sb.release_slots_unchecked(n) };

        self.wait_strategy.notify_all_waiters();
    }
}

impl<T> SequenceBarrierTake for SingleResizingPublisherSequenceBarrier<T>
where
    T: Default + 'static,
{
    unsafe fn take(&mut self) -> T {
        unsafe { self.sb.take().unwrap() }
    }
}

impl<T> InsertSingleConsumer for SingleResizingPublisherSequenceBarrier<T>
where
    T: 'static,
{
    type SingleConsumer = SingleResizingConsumerSequenceBarrier<T>;

    /// See [`InsertSingleConsumer::insert_single_consumer`].
    ///
    /// # Panics
    ///
    /// For now, [this type](Self) only supports a single call to this function, before any items
    /// have been published, and will trigger a panic if called multiple times, or after any items
    /// are released.
    fn insert_single_consumer(&mut self) -> SingleResizingConsumerSequenceBarrier<T> {
        // Prevent this from executing during a transition between buffers, unless/until the
        // implementation is reworked to make support for that possible.
        assert!(
            self.sb.dependencies.sequences.is_empty() && self.sb.sequence.get_owned().value() == 0,
            "The create_consumer_pipeline method can only be called once."
        );

        // Similar to SinglePublisherSequenceBarrier::insert_single_consumer, but the limitations on
        // when this is called result in a simpler implementation, for now. This makes it possible
        // to defer implementing a better solution until after other refactoring is carried out, to
        // avoid doing too much in a single step.

        let new_consumer =
            SingleResizingConsumerSequenceBarrier::new(SingleConsumerSequenceBarrier::new(
                self.sb.ring_buffer.clone(),
                SequenceNumber(SEQUENCE_INITIAL),
                ConsumerDependencies::from_vec(vec![self.sb.sequence.clone_immut()]),
                0,
                self.wait_strategy.clone(),
                self.sb.sequence.clone_immut(),
            ));

        // my_dependencies is an empty Vec now, to be populated with a reference to the new
        // consumer's sequence.
        self.sb.dependencies.sequences.reserve_exact(1);
        self.sb
            .dependencies
            .sequences
            .push(new_consumer.cb.sb.sequence.clone_immut());

        new_consumer
    }
}

/// Resizing-aware consumer barrier.
struct SingleResizingConsumerSequenceBarrier<T> {
    /// Reuse data and constructor from SingleConsumerSequenceBarrier
    cb: SingleConsumerSequenceBarrier<
        ResizableRingBufferArc<ReallocationFlag<T>>,
        TimeoutResizeWaitStrategy,
    >,
}

impl<T> SingleResizingConsumerSequenceBarrier<T> {
    fn new(
        cb: SingleConsumerSequenceBarrier<
            ResizableRingBufferArc<ReallocationFlag<T>>,
            TimeoutResizeWaitStrategy,
        >,
    ) -> SingleResizingConsumerSequenceBarrier<T> {
        SingleResizingConsumerSequenceBarrier { cb }
    }
}

impl<T> LenAvailable for SingleResizingConsumerSequenceBarrier<T>
where
    T: 'static,
{
    fn len_available(&self) -> usize {
        self.cb.len_available()
    }
}

impl<T> PipelineCapacity for SingleResizingConsumerSequenceBarrier<T>
where
    T: 'static,
{
    fn capacity(&self) -> usize {
        self.cb.capacity()
    }
}

impl<T> CurrentSequence for SingleResizingConsumerSequenceBarrier<T>
where
    T: 'static,
{
    fn current_sequence(&self) -> SequenceNumber {
        self.cb.current_sequence()
    }
}

impl<T> Pollable for SingleResizingConsumerSequenceBarrier<T>
where
    T: 'static,
{
    fn poll(&mut self) {
        // If `cached_available` was manually set during a reallocation, calls to this function
        // shouldn't overwrite it with a lower value.
        self.cb.sb.cached_available = cmp::max(
            self.cb.sb.cached_available,
            self.cb
                .sb
                .dependencies
                .calculate_available(self.current_sequence(), self.capacity()),
        );
    }
}

impl<T> SingleResizingConsumerSequenceBarrier<T>
where
    T: 'static,
{
    /// Alters this barrier's sequence to follow the same path that the publisher's took when it
    /// allocated a new buffer. This is necessary when following buffer reallocations to ensure
    /// that downstream consumers take from the same slots that the publisher has written to.
    ///
    /// # Arguments
    ///
    /// * old_buffer_size - The ring buffer's size before the reallocation occurred.
    ///
    fn unwrap_sequence(&mut self, old_buffer_size: usize) {
        let original_sequence = self.current_sequence().value();

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
        let unwrapped_sequence = self.current_sequence().value();
        // Use the real availability value, including the reserved slot
        let current_available = self.cb.len_available();
        let unwrap_difference = unwrapped_sequence - original_sequence;
        let mut actual_cached_available = current_available.wrapping_sub(unwrap_difference);
        // The current cached availability value may be less than the difference if the consumer's
        // sequence has wrapped since it last re-checked availability: the consumer's sequence value
        // is closer to the publisher's before the wrapping occurs, which results in a less inflated
        // availability value. In this case, the value is reset to 1, which forces the actual number
        // of available slots to be refreshed before the next item is processed.
        if current_available <= unwrap_difference {
            actual_cached_available = 1;
        }
        debug!(
            "Adjusting available by {}, from {} to {}. Original sequence: {}, unwrapped: {}",
            unwrap_difference,
            self.len_available(),
            actual_cached_available,
            original_sequence,
            unwrapped_sequence
        );
        self.cb.sb.set_cached_available(actual_cached_available);
    }

    /// Check for a reallocation flag in the slot pointed to by `sequence`. If so, adjust our
    /// sequence to match the change that would have happened to the publisher's sequence, and
    /// adjust the cached availability value to compensate for that jump.
    unsafe fn try_switch_next(&mut self) {
        let old_buffer_size = self.capacity();
        let old_sequence = self.current_sequence();
        // SAFETY: the caller is responsible for ensuring at least one slot is available before
        // calling this.
        let flag = unsafe { self.cb.get() };
        if !flag.is_item() {
            // Switch to newly allocated buffer
            // SAFETY: the flag has established that the buffer was reallocated. If the caller
            // called this correctly, then a happens-before relationship has been established.
            self.cb.sb.ring_buffer = unsafe { self.cb.sb.ring_buffer.get_next() };
            // This is necessary to dereference the same slots that the publisher has written to.
            // In other words, downstream consumers must retrace the publisher's steps.
            self.unwrap_sequence(old_buffer_size);
            debug!(
                "Following switch, sequence: {:?}, unwrapped_sequence: {:?}",
                old_sequence,
                self.current_sequence()
            );
        }
    }
}

impl<T> WaitForSlots for SingleResizingConsumerSequenceBarrier<T>
where
    T: 'static,
{
    // Unfortunately, the resizing scheme removes the ability to guarantee that more than one slot
    // is actually available after polling, because the next slot could be the last one that was
    // published in the current buffer. This is fixable, but for now, just disable support for
    // larger batch sizes.
    fn wait_for_slots(&mut self, min_available: usize) {
        assert!(
            min_available == 1,
            "Batch sizes larger than 1 are currently not supported with resizable buffers."
        );
        self.cb.wait_for_slots(1)
    }
}

impl<T> ReleaseSlots for SingleResizingConsumerSequenceBarrier<T>
where
    T: 'static,
{
    unsafe fn release_slots_unchecked(&mut self, n: usize) {
        debug_assert!(n <= self.len_available());
        // SAFETY: delegated to the caller.
        unsafe { self.cb.release_slots_unchecked(n) };
    }
}

impl<T> SequenceBarrier for SingleResizingConsumerSequenceBarrier<T>
where
    T: 'static,
{
    type T = T;

    unsafe fn set(&mut self, value: T) {
        unsafe { self.cb.set(ReallocationFlag::Item(value)) }
    }

    // The get and take functions check for reallocation events, and adjust the passed in sequence
    // and the barrier's sequence as necessary to match the adjustment to the publisher's sequence
    // that occurred at the time of reallocation.

    unsafe fn get(&mut self) -> &T {
        // SAFETY: the caller has established that at least one slot is available.
        let flag = unsafe { self.cb.get() };
        let reallocation_occurred = !flag.is_item();
        if reallocation_occurred {
            // SAFETY: the caller has established a happens-before relationship with the flag being
            // set, while the value of the flag establishes that a reallocation has already
            // occurred.
            unsafe {
                self.try_switch_next();
            }
        }

        // Retrieve the value a second time here, to satisfy the borrow checker.
        // SAFETY: same as the first time.
        let flag = unsafe { self.cb.get() };
        flag.as_ref().unwrap()
    }
}

impl<T> SequenceBarrierTake for SingleResizingConsumerSequenceBarrier<T>
where
    T: Default + 'static,
{
    unsafe fn take(&mut self) -> T {
        // SAFETY: caller has established that at least one item is available.
        unsafe {
            self.try_switch_next();
        }
        // SAFETY: caller has established that at least one item is available, and this type doesn't
        // allow shared access to slots at its pipeline stage.
        let flag = unsafe { self.cb.take() };
        // After calling try_switch_next, it should be guaranteed that flag holds a real value,
        // whether or not the slot in the buffer was indicating that a reallocation occurred.
        flag.unwrap()
    }
}

impl<T> InsertSingleConsumer for SingleResizingConsumerSequenceBarrier<T> {
    type SingleConsumer = Self;

    fn insert_single_consumer(&mut self) -> Self {
        Self {
            cb: self.cb.insert_single_consumer(),
        }
    }
}

/// Default timeout, in milliseconds, after which publishers will allocate a new ring buffer instead
/// of continuing to wait. This value was chosen with a strong preference for avoiding false
/// positives, even if it means waiting a bit longer in cases where the caller has created a
/// deadlock.
pub const DEFAULT_RESIZE_TIMEOUT: u64 = 500;

/// A wait strategy that acts like BlockingWaitStrategy, except that the publisher gives up after a
/// specified length of time and instead allocates a larger buffer to publish items into.
///
/// Wait strategies other than BlockingWaitStrategy are meant for performance-critical applications,
/// where using automatic resizing would not have made sense. As such, there would have been little
/// value in designing this type to work with other wait strategies.
struct TimeoutResizeWaitStrategy {
    /// Time (in milliseconds) that the publisher should wait for the pipeline to start moving before
    /// assuming that there is a deadlock and allocating a larger buffer.
    timeout: u64,
    /// Fallback wait strategy. See the constructor documentation for details about how it's used.
    wait_strategy: BlockingWaitStrategy,
}

impl TimeoutResizeWaitStrategy {
    /// Construct a new TimeoutResizeWaitStrategy, using `timeout_msecs` to decide when to resize, and
    /// `wait_strategy` to implement the consumer waiting. The wait strategy is also used to
    /// configure how long the publisher's task should spin when waiting for consumers, before
    /// backing off to yielding.
    fn new_with_timeout(
        timeout_msecs: u64,
        wait_strategy: BlockingWaitStrategy,
    ) -> TimeoutResizeWaitStrategy {
        TimeoutResizeWaitStrategy {
            timeout: timeout_msecs,
            wait_strategy,
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
    /// Similar to [`wait_for_dependencies`](PollingWaitStrategy::wait_for_dependencies), except
    /// that it may finish before the requested number of slots are available, returning a value
    /// that is less than `min_available`. If this happens, the caller can reallocate a larger
    /// buffer and start publishing items into that buffer instead of waiting.
    fn try_wait_for_consumers(&self, pollable: &mut dyn Pollable, min_available: usize) {
        if pollable.len_available() >= min_available {
            return;
        }

        // Not enough slots are available. Spin up to max_spin_tries_consumer times, then yield
        // repeatedly until either the items become available, or the timeout is reached.
        let timeout = Duration::from_millis(self.timeout);
        let mut end_time = Instant::now() + timeout;

        spin_for_pollable_with_retries(
            pollable,
            min_available,
            Some(self.wait_strategy.max_spin_tries_consumer),
            |_| spin_loop(),
        );

        if pollable.len_available() >= min_available {
            return;
        }

        let mut previous_available = pollable.len_available();
        while Instant::now() < end_time {
            pollable.poll();

            if pollable.len_available() >= min_available {
                return;
            }

            // Reset the timeout if the pipeline has made progress.There should only be
            // reallocations if it looks like the pipeline has deadlocked.
            if previous_available != pollable.len_available() {
                previous_available = pollable.len_available();
                end_time = Instant::now() + timeout;
            }

            thread::yield_now();
        }
    }
}

impl NotificationWaitStrategy for TimeoutResizeWaitStrategy {
    fn wait_for_publisher(&mut self, pollable: &mut dyn Pollable, min_available: usize) {
        // Consumers wait as normal
        self.wait_strategy
            .wait_for_publisher(pollable, min_available)
    }
    fn notify_all_waiters(&mut self) {
        self.wait_strategy.notify_all_waiters();
    }
}

impl PollingWaitStrategy for TimeoutResizeWaitStrategy {
    fn wait_for_dependencies(&self, pollable: &mut dyn Pollable, min_available: usize) {
        // This code path should be unused
        self.wait_strategy
            .wait_for_dependencies(pollable, min_available)
    }
}

impl fmt::Debug for TimeoutResizeWaitStrategy {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "TimeoutResizeWaitStrategy{{t: {}, p: {}, c: {}}}",
            self.timeout,
            self.wait_strategy.max_spin_tries_publisher,
            self.wait_strategy.max_spin_tries_consumer
        )
    }
}

pub struct SinglePublisher<T, const N: usize, W> {
    p: GenericPublisher<SinglePublisherSequenceBarrier<RingBufferArc<T, N>, W>>,
}

pub struct SharedConsumer<T, const N: usize, W> {
    c: GenericSharedConsumer<SingleConsumerSequenceBarrier<RingBufferArc<T, N>, W>>,
}

pub struct SingleConsumer<T, const N: usize, W> {
    c: GenericSingleConsumer<SingleConsumerSequenceBarrier<RingBufferArc<T, N>, W>>,
}

impl<T, const N: usize, W> SinglePublisher<T, N, W>
where
    T: Default,
{
    /// Constructs a new (non-resizeable) ring buffer with _size_ elements and wraps it into a new
    /// SinglePublisher object.
    pub fn new(wait_strategy: W) -> SinglePublisher<T, N, W> {
        let ring_buffer = RingBufferArc::new();
        let sb = SinglePublisherSequenceBarrier::new(ring_buffer, wait_strategy);
        let gp = GenericPublisher::new_common(sb);
        SinglePublisher { p: gp }
    }
}

impl<T, const N: usize, W> InsertSingleConsumer for SinglePublisher<T, N, W>
where
    T: 'static,
    usize: PowerOfTwoUsize<N>,
    W: Clone,
{
    type SingleConsumer = SingleConsumer<T, N, W>;

    fn insert_single_consumer(&mut self) -> Self::SingleConsumer {
        Self::SingleConsumer {
            c: self.p.insert_single_consumer(),
        }
    }
}

impl<T, const N: usize, W> Publisher<T> for SinglePublisher<T, N, W>
where
    T: 'static,
    usize: PowerOfTwoUsize<N>,
    W: NotificationWaitStrategy,
{
    // In the worst case (minimal microbenchmarking), call overhead is significant.
    #[inline]
    fn publish(&self, value: T) {
        self.p.publish(value)
    }
}

impl<T, const N: usize, W> Consumer<T> for SharedConsumer<T, N, W>
where
    T: 'static,
    usize: PowerOfTwoUsize<N>,
    W: NotificationWaitStrategy,
{
    fn consume<C>(&self, consume_callback: C)
    where
        C: FnMut(&T),
    {
        self.c.consume(consume_callback)
    }
}

impl<T, const N: usize, W> InsertSingleConsumer for SingleConsumer<T, N, W>
where
    W: Clone,
{
    type SingleConsumer = Self;

    fn insert_single_consumer(&mut self) -> Self::SingleConsumer {
        Self {
            c: self.c.insert_single_consumer(),
        }
    }
}

impl<T, const N: usize, W> Consumer<T> for SingleConsumer<T, N, W>
where
    T: 'static,
    usize: PowerOfTwoUsize<N>,
    W: NotificationWaitStrategy,
{
    fn consume<C>(&self, consume_callback: C)
    where
        C: FnMut(&T),
    {
        self.c.consume(consume_callback)
    }
}

impl<T, const N: usize, W> ConsumerMut<T> for SingleConsumer<T, N, W>
where
    T: Default + 'static,
    usize: PowerOfTwoUsize<N>,
    W: NotificationWaitStrategy,
{
    fn take(&self) -> T {
        self.c.take()
    }
}

pub struct SingleResizingPublisher<T> {
    p: GenericPublisher<SingleResizingPublisherSequenceBarrier<T>>,
}

pub struct SharedResizingConsumer<T> {
    c: GenericSharedConsumer<SingleResizingConsumerSequenceBarrier<T>>,
}

pub struct SingleResizingConsumer<T> {
    c: GenericSingleConsumer<SingleResizingConsumerSequenceBarrier<T>>,
}

/// Specialization for resizable ring buffer.
impl<T> SingleResizingPublisher<T>
where
    T: Default,
{
    /// Create a new publisher using a resizable ring buffer, specifying the timeout after which the
    /// publisher will allocate a larger buffer to publish items into.
    ///
    /// # Arguments
    ///
    /// * resize_timeout - How long to wait, in milliseconds, before reallocating a larger buffer
    /// * max_spin_tries_publisher - See `YieldWaitStrategy::new_with_retry_count`
    /// * max_spin_tries_consumer - See `YieldWaitStrategy::new_with_retry_count`
    pub fn new_resize_after_timeout_with_params(
        size: usize,
        resize_timeout: u64,
        max_spin_tries_publisher: usize,
        max_spin_tries_consumer: usize,
    ) -> SingleResizingPublisher<T> {
        let ring_buffer = ResizableRingBufferArc::<ReallocationFlag<T>>::new(size);

        let blocking_wait_strategy = BlockingWaitStrategy::new_with_retry_count(
            max_spin_tries_publisher,
            max_spin_tries_consumer,
        );
        let wait_strategy =
            TimeoutResizeWaitStrategy::new_with_timeout(resize_timeout, blocking_wait_strategy);
        let sb = SingleResizingPublisherSequenceBarrier::new(ring_buffer, wait_strategy);
        let gp = GenericPublisher::new_common(sb);
        SingleResizingPublisher { p: gp }
    }

    /// Construct a TimeoutResizeWaitStrategy using the default parameters.
    pub fn new_resize_after_timeout(size: usize) -> SingleResizingPublisher<T> {
        SingleResizingPublisher::new_resize_after_timeout_with_params(
            size,
            DEFAULT_RESIZE_TIMEOUT,
            DEFAULT_MAX_SPIN_TRIES_PUBLISHER,
            DEFAULT_MAX_SPIN_TRIES_CONSUMER,
        )
    }
}

impl<T> InsertSingleConsumer for SingleResizingPublisher<T>
where
    T: 'static,
{
    type SingleConsumer = SingleResizingConsumer<T>;

    /// See [`InsertSingleConsumer::insert_single_consumer`].
    ///
    /// # Panics
    ///
    /// For now, the resizing variant only supports a single call to this function, before any items
    /// have been published, and will trigger a panic if called multiple times, or after any items
    /// are released.
    fn insert_single_consumer(&mut self) -> Self::SingleConsumer {
        SingleResizingConsumer {
            c: self.p.insert_single_consumer(),
        }
    }
}

impl<T> Publisher<T> for SingleResizingPublisher<T>
where
    T: Default + 'static,
{
    // In the worst case (minimal microbenchmarking), call overhead is significant.
    #[inline]
    fn publish(&self, value: T) {
        self.p.publish(value)
    }
}

impl<T> Consumer<T> for SharedResizingConsumer<T>
where
    T: 'static,
{
    fn consume<C>(&self, consume_callback: C)
    where
        C: FnMut(&T),
    {
        self.c.consume(consume_callback)
    }
}

impl<T> InsertSingleConsumer for SingleResizingConsumer<T> {
    type SingleConsumer = Self;

    fn insert_single_consumer(&mut self) -> Self::SingleConsumer {
        Self {
            c: self.c.insert_single_consumer(),
        }
    }
}

impl<T> Consumer<T> for SingleResizingConsumer<T>
where
    T: 'static,
{
    fn consume<C>(&self, consume_callback: C)
    where
        C: FnMut(&T),
    {
        self.c.consume(consume_callback)
    }
}

impl<T> ConsumerMut<T> for SingleResizingConsumer<T>
where
    T: Default + 'static,
{
    fn take(&self) -> T {
        self.c.take()
    }
}

#[cfg(test)]
mod resizing_tests {
    use crate::{
        Consumer, ConsumerMut, InsertSingleConsumer, Publisher, SingleResizingPublisher,
        wrap_boundary,
    };

    #[test_log::test]
    fn resizing() {
        const CAPACITY: usize = 8;
        let mut publisher = SingleResizingPublisher::new_resize_after_timeout(CAPACITY);
        let mut final_consumer = publisher.insert_single_consumer();

        let consumer = final_consumer.insert_single_consumer();

        let mut next_item_publish = 1;
        let mut next_item_consume = 1;
        let mut next_item_take = 1;

        // Test several simultaneously pending reallocations
        const MAX_CAPACITY: usize = CAPACITY * 2 * 2 * 2;
        const ITEMS_IN_FLIGHT: usize = MAX_CAPACITY + 1;
        #[allow(clippy::explicit_counter_loop)]
        for _ in 0..ITEMS_IN_FLIGHT {
            publisher.publish(next_item_publish);
            next_item_publish += 1;
        }
        for _ in 0..ITEMS_IN_FLIGHT {
            consumer.consume(|item| {
                assert!(*item == next_item_consume);
                next_item_consume += 1;
            });
        }
        for _ in 0..ITEMS_IN_FLIGHT {
            assert!(next_item_take == final_consumer.take());
            next_item_take += 1;
        }

        // Also test wrapping in publisher availability calculation, at some level, by running all
        // three stages in lockstep with each other.
        for _ in 0..wrap_boundary(MAX_CAPACITY) {
            publisher.publish(next_item_publish);
            next_item_publish += 1;
            consumer.consume(|item| {
                assert!(*item == next_item_consume);
                next_item_consume += 1;
            });
            assert!(next_item_take == final_consumer.take());
            next_item_take += 1;
        }
    }
}
