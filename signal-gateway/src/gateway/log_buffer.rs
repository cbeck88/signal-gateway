//! Thread-safe log message buffer.
//!
//! Wraps a circular buffer with a synchronous mutex for fast, blocking access.

use super::circular_buffer::CircularBuffer;
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

    /// Iterate over all messages without modifying the buffer.
    ///
    /// Messages are passed to `f` in reverse order (newest first).
    pub fn for_each(&self, mut f: impl FnMut(&LogMessage)) {
        let buf = self.buf.lock().unwrap();
        for log_msg in buf.iter().rev() {
            f(log_msg);
        }
    }

    /// Returns the number of messages currently in the buffer.
    pub fn len(&self) -> usize {
        self.buf.lock().unwrap().len()
    }
}
