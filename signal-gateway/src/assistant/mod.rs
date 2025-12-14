//! Assistant integration for AI-powered responses.
//!
//! This module provides a channel-based wrapper around an `Assistant` implementation,
//! handling message queuing and background processing.

mod worker;

// Re-export the assistant types that callers need
pub use signal_gateway_assistant::{
    Assistant, AssistantResponse, ChatMessage, SentBy, Tool, ToolExecutor, ToolResult,
};

use crate::message_handler::AdminMessageResponse;
use chrono::{DateTime, Utc};
use tokio::sync::{mpsc, oneshot};
use worker::{AssistantWorker, Input};

/// Size of the request queue for the assistant worker.
const REQUEST_QUEUE_SIZE: usize = 16;

/// Error type for assistant operations at the gateway level.
#[derive(Debug, thiserror::Error)]
pub enum AssistantError {
    /// Request queue is full.
    #[error("request queue is full")]
    QueueFull,
    /// Worker has shut down.
    #[error("worker has shut down")]
    WorkerGone,
    /// Request was cancelled.
    #[error("request cancelled")]
    Cancelled,
}

/// Assistant agent that processes requests via a background worker.
///
/// Requests are processed serially by a background worker to prevent
/// concurrent API calls.
pub struct AssistantAgent {
    input_tx: mpsc::Sender<Input>,
    stop_tx: mpsc::Sender<()>,
    #[allow(dead_code)]
    worker_handle: tokio::task::JoinHandle<()>,
}

impl AssistantAgent {
    /// Create a new assistant agent with the given assistant implementation.
    ///
    /// Spawns a background worker task that processes requests serially.
    pub fn new(assistant: Box<dyn Assistant>) -> Self {
        let (input_tx, input_rx) = mpsc::channel(REQUEST_QUEUE_SIZE);
        let (stop_tx, stop_rx) = mpsc::channel(REQUEST_QUEUE_SIZE);

        let worker = AssistantWorker::new(assistant, input_rx, stop_rx);

        let worker_handle = tokio::spawn(async move {
            worker.run().await;
            tracing::info!("Assistant worker task exited");
        });

        Self {
            input_tx,
            stop_tx,
            worker_handle,
        }
    }

    /// Send a prompt and wait for a response.
    ///
    /// Returns `QueueFull` error if the request queue is full.
    pub async fn request(
        &self,
        prompt: &str,
        ts_ms: u64,
    ) -> Result<AdminMessageResponse, AssistantError> {
        let (result_tx, result_rx) = oneshot::channel();

        let timestamp = DateTime::from_timestamp_millis(ts_ms as i64).unwrap_or_else(Utc::now);
        let msg = ChatMessage {
            sent_by: SentBy::UserToAssistant,
            text: prompt.into(),
            timestamp,
        };

        self.input_tx
            .try_send(Input::Prompt(msg, result_tx))
            .map_err(|_| AssistantError::QueueFull)?;

        result_rx.await.map_err(|_| AssistantError::WorkerGone)?
    }

    /// Record a message in the assistant's history without expecting a response.
    pub fn record_message(&self, sent_by: SentBy, text: &str, ts_ms: u64) {
        let timestamp = DateTime::from_timestamp_millis(ts_ms as i64).unwrap_or_else(Utc::now);
        let msg = ChatMessage {
            sent_by,
            text: text.into(),
            timestamp,
        };

        let _ = self.input_tx.try_send(Input::Record(msg));
    }

    /// Request the worker to compact the assistant's message history.
    pub fn request_compaction(&self) {
        let _ = self.input_tx.try_send(Input::Compact);
    }

    /// Request the worker to log its state for debugging.
    pub fn request_debug(&self) {
        let _ = self.input_tx.try_send(Input::Debug);
    }

    /// Request the worker to stop processing and cancel pending requests.
    pub fn request_stop(&self) {
        let _ = self.stop_tx.try_send(());
    }
}
