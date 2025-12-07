//! Claude API integration for AI-powered responses with tool use support.

mod tools;
pub use tools::{Tool, ToolExecutor};

use conf::Conf;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Weak;
use tracing::info;

/// The Anthropic API version header value. This is a stable version identifier,
/// not a date indicating when the API was released. Anthropic adds new features
/// through separate `anthropic-beta` headers rather than bumping this version.
/// The official Anthropic SDKs use this same value.
const ANTHROPIC_API_VERSION: &str = "2023-06-01";

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
    /// Tool execution failed.
    #[error("tool execution failed: {0}")]
    ToolError(String),
    /// Too many tool use iterations.
    #[error("exceeded maximum tool use iterations ({0})")]
    TooManyIterations(u32),
    /// Tool executor is no longer available.
    #[error("tool executor is gone")]
    ToolExecutorGone,
}

/// Claude API client.
pub struct ClaudeApi {
    config: ClaudeConfig,
    client: reqwest::Client,
    api_key: String,
    system_prompt: String,
    tool_executor: Weak<dyn ToolExecutor>,
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
    pub fn new(
        config: ClaudeConfig,
        tool_executor: Weak<dyn ToolExecutor>,
    ) -> Result<Self, ClaudeError> {
        let api_key = std::fs::read_to_string(&config.api_key_file)
            .map_err(ClaudeError::ApiKeyRead)?
            .trim()
            .to_owned();

        let system_prompt = std::fs::read_to_string(&config.system_prompt_file)
            .map_err(ClaudeError::SystemPromptRead)?;

        let client = reqwest::Client::new();

        Ok(Self {
            config,
            client,
            api_key,
            system_prompt,
            tool_executor,
        })
    }

    /// Send a request to the Claude API and return the response text.
    /// If a tool executor has been installed, handles tool use in a loop.
    pub async fn request(&self, prompt: &str) -> Result<String, ClaudeError> {
        let max_iterations = self.config.claude_max_iterations;

        // Get tools from the executor if still alive
        let executor = self.tool_executor.upgrade();
        let tools = executor
            .as_ref()
            .map(|te| te.tools())
            .unwrap_or_default();
        let mut messages = vec![MessageContent::user(prompt)];

        info!("Claude request: {}", prompt);

        for iteration in 0..max_iterations {
            let request_body = MessagesRequest {
                model: &self.config.claude_model,
                max_tokens: self.config.claude_max_tokens,
                system: &self.system_prompt,
                messages: messages.clone(),
                tools: tools.clone(),
            };

            let response = self
                .client
                .post(&self.config.claude_api_url)
                .header("x-api-key", &self.api_key)
                .header("anthropic-version", ANTHROPIC_API_VERSION)
                .header("content-type", "application/json")
                .json(&request_body)
                .send()
                .await?;

            if !response.status().is_success() {
                let error: ErrorResponse = response.json().await?;
                return Err(ClaudeError::ApiError(error.error.message));
            }

            let response: MessagesResponse = response.json().await?;
            info!(
                "Claude response (stop_reason={}): {:?}",
                response.stop_reason, response.content
            );

            // Check if we need to handle tool use
            if response.stop_reason == "tool_use" {
                // Try to get a strong reference to the executor
                let Some(executor) = self.tool_executor.upgrade() else {
                    return Err(ClaudeError::ToolExecutorGone);
                };

                // Add assistant's response to messages
                messages.push(MessageContent::assistant(response.content.clone()));

                // Execute each tool use and collect results
                for block in &response.content {
                    if let ContentBlock::ToolUse { id, name, input } = block {
                        info!("Claude tool use: {}({})", name, input);
                        let (result, is_error) = match executor.execute(name, input).await {
                            Ok(result) => {
                                info!("Tool result: {}", result);
                                (result, false)
                            }
                            Err(err) => {
                                info!("Tool error: {}", err);
                                (err, true)
                            }
                        };
                        messages.push(MessageContent::tool_result(id.clone(), result, is_error));
                    }
                }

                // Continue the loop to get Claude's next response
                info!("Tool use iteration {}, continuing...", iteration + 1);
                continue;
            }

            // No tool use, extract final text response
            let text = response
                .content
                .into_iter()
                .filter_map(|block| {
                    if let ContentBlock::Text { text } = block {
                        Some(text)
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");

            info!("Claude final result: {}", text);
            return Ok(text);
        }

        Err(ClaudeError::TooManyIterations(max_iterations))
    }
}
