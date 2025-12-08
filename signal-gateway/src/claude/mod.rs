//! Claude API integration for AI-powered responses with tool use support.

mod tools;
mod worker;

pub use tools::{Tool, ToolExecutor};

use conf::Conf;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Weak;
use tokio::sync::{mpsc, oneshot};
use worker::{ClaudeRequest, ClaudeWorker};

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
    ApiError(String),
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
    /// Worker returned an error.
    #[error("{0}")]
    WorkerError(String),
}

/// Claude API client.
///
/// Requests are processed serially by a background worker to prevent
/// concurrent API calls.
pub struct ClaudeApi {
    request_tx: mpsc::Sender<ClaudeRequest>,
    #[allow(dead_code)]
    worker_handle: tokio::task::JoinHandle<()>,
}

/// Request body for the Claude Messages API.
#[derive(Serialize)]
struct MessagesRequest<'a> {
    model: &'a str,
    max_tokens: u32,
    system: &'a str,
    messages: Vec<MessageContent>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<Tool>,
}

/// A message in the conversation (can have multiple content blocks).
#[derive(Clone, Debug, Serialize, Deserialize)]
struct MessageContent {
    role: String,
    content: Vec<ContentBlock>,
}

impl MessageContent {
    fn user(text: &str) -> Self {
        Self {
            role: "user".to_owned(),
            content: vec![ContentBlock::Text {
                text: text.to_owned(),
            }],
        }
    }

    fn assistant(blocks: Vec<ContentBlock>) -> Self {
        Self {
            role: "assistant".to_owned(),
            content: blocks,
        }
    }

    fn tool_result(tool_use_id: String, content: String, is_error: bool) -> Self {
        Self {
            role: "user".to_owned(),
            content: vec![ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error: if is_error { Some(true) } else { None },
            }],
        }
    }
}

/// A content block in the request/response.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ContentBlock {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
    },
}

/// Response from the Claude Messages API.
#[derive(Debug, Deserialize)]
struct MessagesResponse {
    content: Vec<ContentBlock>,
    stop_reason: String,
}

/// Error response from the Claude API.
#[derive(Deserialize)]
struct ErrorResponse {
    error: ApiErrorDetail,
}

#[derive(Deserialize)]
struct ApiErrorDetail {
    message: String,
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
        let (request_tx, request_rx) = mpsc::channel(REQUEST_QUEUE_SIZE);

        let worker = ClaudeWorker::new(config, tool_executor, request_rx)?;

        let worker_handle = tokio::spawn(async move {
            worker.run().await;
            tracing::info!("Claude worker task exited");
        });

        Ok(Self {
            request_tx,
            worker_handle,
        })
    }

    /// Send a request to the Claude API and return the response text.
    ///
    /// Returns a future that resolves when the request is complete.
    /// Returns `QueueFull` error if the request queue is full.
    pub async fn request(&self, prompt: &str) -> Result<String, ClaudeError> {
        let (result_tx, result_rx) = oneshot::channel();

        let request = ClaudeRequest {
            prompt: prompt.to_owned(),
            result_sender: result_tx,
        };

        self.request_tx
            .try_send(request)
            .map_err(|_| ClaudeError::QueueFull)?;

        result_rx
            .await
            .map_err(|_| ClaudeError::WorkerGone)?
            .map_err(ClaudeError::WorkerError)
    }
}
