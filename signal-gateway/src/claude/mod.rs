//! Claude API integration for AI-powered responses with tool use support.

mod tools;
mod worker;

pub use tools::{Tool, ToolExecutor, ToolResult};
pub use worker::SentBy;

use crate::message_handler::AdminMessageResponse;
use chrono::{DateTime, Utc};
use conf::Conf;
use std::path::PathBuf;
use std::sync::Weak;
use tokio::sync::{mpsc, oneshot};
use worker::{ChatMessage, ClaudeWorker, Input};

/// The Anthropic API version header value. This is a stable version identifier,
/// not a date indicating when the API was released. Anthropic adds new features
/// through separate `anthropic-beta` headers rather than bumping this version.
/// The official Anthropic SDKs use this same value.
const ANTHROPIC_API_VERSION: &str = "2023-06-01";

/// Size of the request queue for the Claude worker.
const REQUEST_QUEUE_SIZE: usize = 16;

/// Configuration for the Claude API integration.
#[derive(Clone, Conf, Debug)]
#[conf(serde)]
pub struct ClaudeConfig {
    /// Path to file containing the Claude API key.
    #[conf(long, env)]
    pub api_key_file: PathBuf,
    /// Path to file containing the system prompt.
    #[conf(long, env)]
    pub system_prompt_file: PathBuf,
    /// Claude API URL.
    #[conf(long, env, default_value = "https://api.anthropic.com/v1/messages")]
    pub claude_api_url: String,
    /// Claude model to use.
    #[conf(long, env, default_value = "claude-opus-4-20250514")]
    pub claude_model: String,
    /// Maximum tokens in the response.
    #[conf(long, env, default_value = "1024")]
    pub claude_max_tokens: u32,
    /// Maximum tool use iterations before giving up.
    #[conf(long, env, default_value = "10")]
    pub claude_max_iterations: u32,
}

/// Error type for Claude API operations.
#[derive(Debug, thiserror::Error)]
pub enum ClaudeError {
    /// Failed to read API key file.
    #[error("failed to read API key file: {0}")]
    ApiKeyRead(std::io::Error),
    /// Failed to read system prompt file.
    #[error("failed to read system prompt file: {0}")]
    SystemPromptRead(std::io::Error),
    /// HTTP request failed.
    #[error("HTTP request failed: {0}")]
    Request(#[from] reqwest::Error),
    /// API returned an error response.
    #[error("API error: {0}")]
    ApiError(Box<str>),
    /// Too many tool use iterations.
    #[error("exceeded maximum tool use iterations ({0})")]
    TooManyIterations(u32),
    /// Tool executor is no longer available.
    #[error("tool executor is gone")]
    ToolExecutorGone,
    /// Request queue is full.
    #[error("request queue is full")]
    QueueFull,
    /// Worker has shut down.
    #[error("worker has shut down")]
    WorkerGone,
    /// Stop was requested.
    #[error("stop requested")]
    StopRequested,
}

/// Claude API client.
///
/// Requests are processed serially by a background worker to prevent
/// concurrent API calls.
pub struct ClaudeApi {
    input_tx: mpsc::Sender<Input>,
    stop_tx: mpsc::Sender<()>,
    #[allow(dead_code)]
    worker_handle: tokio::task::JoinHandle<()>,
}

impl ClaudeApi {
    /// Create a new Claude API client from configuration.
    ///
    /// Reads the API key and system prompt from the configured files.
    /// The tool executor is held as a weak reference, so it will not prevent
    /// the executor from being dropped. If the executor is dropped during a
    /// request, tool use will fail with `ToolExecutorGone`.
    ///
    /// Spawns a background worker task that processes requests serially.
    pub fn new(
        config: ClaudeConfig,
        tool_executor: Weak<dyn ToolExecutor>,
    ) -> Result<Self, ClaudeError> {
        let (input_tx, input_rx) = mpsc::channel(REQUEST_QUEUE_SIZE);
        let (stop_tx, stop_rx) = mpsc::channel(REQUEST_QUEUE_SIZE);

        let worker = ClaudeWorker::new(config, tool_executor, input_rx, stop_rx)?;

        let worker_handle = tokio::spawn(async move {
            worker.run().await;
            tracing::info!("Claude worker task exited");
        });

        Ok(Self {
            input_tx,
            stop_tx,
            worker_handle,
        })
    }

    /// Send a prompt to the Claude API, execute tools etc., and return the response.
    ///
    /// Returns a future that resolves when the request is complete.
    /// Returns `QueueFull` error if the request queue is full.
    pub async fn request(
        &self,
        prompt: &str,
        ts_ms: u64,
    ) -> Result<AdminMessageResponse, ClaudeError> {
        let (result_tx, result_rx) = oneshot::channel();

        let timestamp = DateTime::from_timestamp_millis(ts_ms as i64).unwrap_or_else(Utc::now);
        let msg = ChatMessage {
            sent_by: SentBy::UserToClaude,
            text: prompt.into(),
            timestamp,
            result_sender: Some(result_tx),
        };

        self.input_tx
            .try_send(Input::Chat(msg))
            .map_err(|_| ClaudeError::QueueFull)?;

        // result_rx.await has type Result<Result<String, ClaudeError>, RecvError>
        // The outer Result is for channel errors, the inner is the actual response
        result_rx.await.map_err(|_| ClaudeError::WorkerGone)?
    }

    /// Record a message into claude's chat log that claude is not expected to respond to
    pub fn record_message(&self, sent_by: SentBy, text: &str, ts_ms: u64) {
        let timestamp = DateTime::from_timestamp_millis(ts_ms as i64).unwrap_or_else(Utc::now);
        let msg = ChatMessage {
            sent_by,
            text: text.into(),
            timestamp,
            result_sender: None,
        };

        let _ = self.input_tx.try_send(Input::Chat(msg));
    }

    /// Request the worker to perform compaction (summarizing their recent message history and discarding it)
    pub fn request_compaction(&self) {
        let _ = self.input_tx.try_send(Input::Compact);
    }

    /// Request the worker to interrupt any prompts it is responding to, discard any queued messages,
    /// and get ready to accept different prompts.
    ///
    /// This will cause the current request (if any) to be interrupted at the
    /// next opportunity, and all pending requests to receive `StopRequested` errors.
    pub fn request_stop(&self) {
        let _ = self.stop_tx.try_send(());
    }
}
