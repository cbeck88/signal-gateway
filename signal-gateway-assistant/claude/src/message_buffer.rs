//! Message buffer with cached character count.

use serde_json::Value;
use std::collections::VecDeque;
use std::fmt;

use crate::{ContentBlock, MessageContent};

/// A buffer of messages with cached total character count.
///
/// Uses `VecDeque` for efficient front removal during compaction.
pub struct MessageBuffer {
    messages: VecDeque<MessageContent>,
    /// Cached total character count of all messages.
    total_chars: usize,
}

impl MessageBuffer {
    /// Create a new empty message buffer.
    pub fn new() -> Self {
        Self {
            messages: VecDeque::new(),
            total_chars: 0,
        }
    }

    /// Push a message to the back of the buffer.
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

    /// Returns a reference to the last message, if any.
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

impl Default for MessageBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for MessageBuffer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MessageBuffer")
            .field("len", &self.messages.len())
            .field("total_chars", &self.total_chars)
            .field("messages", &self.messages)
            .finish()
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

/// Estimate the serialized JSON size of a value.
fn estimate_json_size(value: &Value) -> usize {
    match value {
        Value::Null => 4,
        Value::Bool(true) => 4,
        Value::Bool(false) => 5,
        Value::Number(n) => n.to_string().len(),
        Value::String(s) => s.len() + 2,
        Value::Array(arr) => {
            2 + arr.iter().map(estimate_json_size).sum::<usize>() + arr.len().saturating_sub(1)
        }
        Value::Object(obj) => {
            2 + obj
                .iter()
                .map(|(k, v)| k.len() + 3 + estimate_json_size(v))
                .sum::<usize>()
                + obj.len().saturating_sub(1)
        }
    }
}
