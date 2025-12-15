//! Message buffer with cached character count.

use crate::api::{ContentBlock, MessageContent};
use serde_json::Value;
use std::collections::VecDeque;

/// A buffer of messages with cached total character count.
#[derive(Clone, Debug, Default)]
pub struct MessageBuffer {
    /// `VecDeque` to easily remove oldest messages if needed
    messages: VecDeque<MessageContent>,
    /// Cached total character count of all messages.
    total_chars: usize,
}

impl MessageBuffer {
    /// Create a new empty message buffer.
    pub fn new() -> Self {
        Self {
            messages: VecDeque::with_capacity(256),
            total_chars: 0,
        }
    }

    /// Push a message to the back.
    pub fn push(&mut self, msg: MessageContent) {
        self.total_chars += message_chars(&msg);
        self.messages.push_back(msg);
    }

    /// Remove and return the first message, if any.
    pub fn pop_front(&mut self) -> Option<MessageContent> {
        let msg = self.messages.pop_front()?;
        self.total_chars = self.total_chars.saturating_sub(message_chars(&msg));
        Some(msg)
    }

    /// Clear all messages from the buffer.
    pub fn clear(&mut self) {
        self.messages.clear();
        self.total_chars = 0;
    }

    /// Returns true if the buffer is empty.
    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    /// Returns the number of messages in the buffer.
    pub fn len(&self) -> usize {
        self.messages.len()
    }

    /// Returns the cached total character count.
    pub fn total_chars(&self) -> usize {
        self.total_chars
    }

    /// Get the last message, if any.
    pub fn last(&self) -> Option<&MessageContent> {
        self.messages.back()
    }

    /// Returns a slice of all messages for API requests.
    ///
    /// Note: This may require making the deque contiguous first.
    pub fn make_contiguous(&mut self) -> &[MessageContent] {
        self.messages.make_contiguous()
    }
}

/// Calculate the character count for a single message.
fn message_chars(msg: &MessageContent) -> usize {
    msg.content
        .iter()
        .map(|block| match block {
            ContentBlock::Text { text } => text.len(),
            ContentBlock::ToolUse { input, .. } => estimate_json_size(input),
            ContentBlock::ToolResult { content, .. } => content.len(),
        })
        .sum()
}

// Estimate the serialized JSON size of a value.
// https://github.com/serde-rs/json/issues/784#issuecomment-877688512
fn estimate_json_size(value: &Value) -> usize {
    use serde::Serialize;
    use std::io::{Result, Write};

    struct ByteCount(usize);

    impl Write for ByteCount {
        fn write(&mut self, buf: &[u8]) -> Result<usize> {
            self.0 += buf.len();
            Ok(buf.len())
        }
        fn flush(&mut self) -> Result<()> {
            Ok(())
        }
    }

    let mut ser = serde_json::Serializer::new(ByteCount(0));
    value.serialize(&mut ser).unwrap();
    ser.into_inner().0
}
