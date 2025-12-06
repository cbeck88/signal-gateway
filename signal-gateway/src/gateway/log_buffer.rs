//! Thread-safe log message buffer.
//!
//! Wraps a circular buffer with a synchronous mutex for fast, blocking access.

use crate::circular_buffer::CircularBuffer;
use crate::log_message::LogMessage;
use std::sync::Mutex;

/// A thread-safe circular buffer for log messages.
///
/// Uses a synchronous mutex since all operations are fast and non-blocking.
#[derive(Debug)]
pub struct LogBuffer {
    buf: Mutex<CircularBuffer<LogMessage>>,
}

impl LogBuffer {
    /// Create a new log buffer with the given capacity.
    pub fn new(capacity: usize) -> Self {
        Self {
            buf: Mutex::new(CircularBuffer::new(capacity)),
        }
    }

    /// Push a log message to the buffer.
    pub fn push_back(&self, msg: LogMessage) {
        let mut buf = self.buf.lock().unwrap();
        buf.push_back(msg);
    }

    /// Push a log message and drain all messages, calling `f` for each.
    ///
    /// Messages are passed to `f` in reverse order (newest first).
    /// The buffer is cleared after draining.
    pub fn push_back_and_drain(&self, msg: LogMessage, mut f: impl FnMut(&LogMessage)) {
        let mut buf = self.buf.lock().unwrap();
        buf.push_back(msg);
        for log_msg in buf.iter().rev() {
            f(log_msg);
        }
        buf.clear();
    }

    /// Access the buffer contents via an iterator.
    ///
    /// The iterator yields messages in reverse order (newest first) and
    /// implements `ExactSizeIterator`, so `iter.len()` returns the count.
    // NOTE: We use `&mut dyn ExactSizeIterator` rather than `impl FnOnce(impl ExactSizeIterator)`
    // because Rust doesn't allow nested `impl Trait` in that position. A generic parameter
    // `F: FnOnce(I) where I: ExactSizeIterator` doesn't work either because `I` would be
    // caller-determined, but we need to pass our concrete iterator type. HRTB with the
    // concrete type (`F: for<'a> FnOnce(Rev<vec_deque::Iter<'a, T>>)`) works but leaks
    // implementation details.
    pub fn with_iter<R>(&self, f: impl FnOnce(&mut dyn ExactSizeIterator<Item = &LogMessage>) -> R) -> R {
        let buf = self.buf.lock().unwrap();
        let mut iter = buf.iter().rev();
        f(&mut iter)
    }
}
