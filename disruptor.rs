use std::vec;

struct RingBuffer<T> {
    entries: ~[T],
}

impl<T> RingBuffer<T> {
    pub fn new(size: uint) -> RingBuffer<T> {
        assert!(size.population_count() == 1, "RingBuffer size must be a power of two (received %?)", size);
        RingBuffer { entries: vec::with_capacity(size) }
    }
}

#[should_fail]
#[test]
pub fn ring_buffer_size_must_be_power_of_two_7() {
    RingBuffer::<()>::new(7);
}
#[test]
pub fn ring_buffer_size_must_be_power_of_two_1() {
    RingBuffer::<()>::new(1);
}
#[test]
pub fn ring_buffer_size_must_be_power_of_two_8() {
    RingBuffer::<()>::new(8);
}
