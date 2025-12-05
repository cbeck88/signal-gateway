//! A circular buffer with runtime-determined capacity.
//!
//! This is a thin wrapper around `VecDeque` that enforces a fixed capacity
//! set at initialization time. When pushing to a full buffer, the oldest
//! element is automatically removed.

use std::collections::VecDeque;

/// A circular buffer with a fixed capacity determined at runtime.
///
/// Unlike `VecDeque`, this buffer will never grow beyond its initial capacity.
/// When `push_back` is called on a full buffer, the oldest element is removed first.
#[derive(Debug)]
pub struct CircularBuffer<T> {
    buf: VecDeque<T>,
    capacity: usize,
}

impl<T> CircularBuffer<T> {
    /// Create a new circular buffer with the given capacity.
    ///
    /// # Panics
    /// Panics if capacity is 0.
    pub fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "CircularBuffer capacity must be > 0");
        Self {
            buf: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    /// Push an element to the back of the buffer.
    /// If the buffer is at capacity, the oldest element is removed first.
    pub fn push_back(&mut self, value: T) {
        if self.buf.len() == self.capacity {
            self.buf.pop_front();
        }
        self.buf.push_back(value);
    }

    /// Returns the number of elements in the buffer.
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// Returns true if the buffer is empty.
    #[allow(unused)]
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Returns the capacity of the buffer.
    #[allow(unused)]
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Returns an iterator over the elements, from oldest to newest.
    pub fn iter(&self) -> impl DoubleEndedIterator<Item = &T> {
        self.buf.iter()
    }

    /// Clears the buffer, removing all elements.
    pub fn clear(&mut self) {
        self.buf.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_push_within_capacity() {
        let mut buf = CircularBuffer::new(3);
        buf.push_back(1);
        buf.push_back(2);
        buf.push_back(3);

        assert_eq!(buf.len(), 3);
        assert_eq!(buf.iter().copied().collect::<Vec<_>>(), vec![1, 2, 3]);
    }

    #[test]
    fn test_push_beyond_capacity() {
        let mut buf = CircularBuffer::new(3);
        buf.push_back(1);
        buf.push_back(2);
        buf.push_back(3);
        buf.push_back(4); // Should evict 1

        assert_eq!(buf.len(), 3);
        assert_eq!(buf.iter().copied().collect::<Vec<_>>(), vec![2, 3, 4]);
    }

    #[test]
    fn test_push_many_beyond_capacity() {
        let mut buf = CircularBuffer::new(3);
        for i in 1..=10 {
            buf.push_back(i);
        }

        assert_eq!(buf.len(), 3);
        assert_eq!(buf.iter().copied().collect::<Vec<_>>(), vec![8, 9, 10]);
    }

    #[test]
    fn test_iter_reverse() {
        let mut buf = CircularBuffer::new(3);
        buf.push_back(1);
        buf.push_back(2);
        buf.push_back(3);

        assert_eq!(buf.iter().rev().copied().collect::<Vec<_>>(), vec![3, 2, 1]);
    }

    #[test]
    fn test_clear() {
        let mut buf = CircularBuffer::new(3);
        buf.push_back(1);
        buf.push_back(2);
        buf.push_back(3);
        buf.clear();

        assert!(buf.is_empty());
        assert_eq!(buf.len(), 0);
        assert_eq!(buf.capacity(), 3);
    }

    #[test]
    fn test_capacity_one() {
        let mut buf = CircularBuffer::new(1);
        buf.push_back(1);
        assert_eq!(buf.iter().copied().collect::<Vec<_>>(), vec![1]);

        buf.push_back(2);
        assert_eq!(buf.iter().copied().collect::<Vec<_>>(), vec![2]);
    }

    #[test]
    #[should_panic(expected = "capacity must be > 0")]
    fn test_zero_capacity_panics() {
        let _buf: CircularBuffer<i32> = CircularBuffer::new(0);
    }
}
