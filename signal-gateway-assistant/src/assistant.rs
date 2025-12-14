use crate::{AssistantResponse, ChatMessage};
use std::error::Error;
use tokio_util::sync::CancellationToken;

/// Assistant trait which handles chat messages and commands.
///
/// Implementations handle the actual LLM API calls, message history management,
/// and tool execution.
#[async_trait::async_trait]
pub trait Assistant: Send {
    /// Record a chat message in the assistant's history without expecting a response.
    async fn record_message(&mut self, message: ChatMessage);

    /// Process a chat message and generate a response.
    ///
    /// The cancellation token can be used to interrupt long-running operations.
    /// Returns `Ok(None)` if the operation was cancelled.
    async fn prompt(
        &mut self,
        message: ChatMessage,
        cancel: CancellationToken,
    ) -> Result<Option<AssistantResponse>, Box<dyn Error + Send + Sync>>;

    /// Compact the assistant's message history (e.g., by summarizing).
    ///
    /// This is called when the user explicitly requests compaction.
    /// Automatic compaction is an internal implementation detail.
    ///
    /// The cancellation token can be used to interrupt the operation.
    async fn compact(&mut self, cancel: CancellationToken);

    /// Log the assistant's current state for debugging.
    fn debug_log(&mut self);
}
