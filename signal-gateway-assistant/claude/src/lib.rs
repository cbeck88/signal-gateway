//! Claude API implementation of the Assistant trait.

mod api;
mod message_buffer;

use api::{
    ContentBlock, ErrorResponse, MessageContent, MessagesRequest, MessagesResponse, SystemContent,
};
use conf::Conf;
use message_buffer::MessageBuffer;
use signal_gateway_assistant::{Assistant, AssistantResponse, ChatMessage, ToolExecutor};
use std::{path::PathBuf, sync::Weak, time::Duration};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

/// The Anthropic API version header value.
const ANTHROPIC_API_VERSION: &str = "2023-06-01";

/// Configuration for the Claude API integration.
#[derive(Clone, Conf, Debug)]
#[conf(serde)]
pub struct ClaudeConfig {
    /// Path to file containing the Claude API key.
    #[conf(long, env)]
    pub api_key_file: PathBuf,
    /// Paths to files containing system prompt components (in order, last one is cached).
    #[conf(repeat, long, env)]
    pub system_prompt_files: Vec<PathBuf>,
    /// Claude API URL.
    #[conf(long, env, default_value = "https://api.anthropic.com/v1/messages")]
    pub claude_api_url: String,
    /// Claude model to use.
    #[conf(long, env, default_value = "claude-sonnet-4-5-20250929")]
    pub claude_model: String,
    /// Maximum tokens in the response.
    #[conf(long, env, default_value = "1024")]
    pub claude_max_tokens: u32,
    /// Maximum tool use iterations before giving up.
    #[conf(long, env, default_value = "10")]
    pub claude_max_iterations: u32,
    /// Enable prompt caching (adds cache_control to system prompts).
    #[conf(long, env)]
    pub prompt_caching: bool,
    /// Compaction configuration.
    #[conf(flatten, prefix)]
    pub compaction: CompactionConfig,
}

/// Configuration for message buffer compaction.
#[derive(Clone, Conf, Debug)]
#[conf(serde)]
pub struct CompactionConfig {
    /// Path to file containing the compaction prompt.
    #[conf(long, env)]
    pub prompt_file: PathBuf,
    /// Model to use for compaction (typically a faster/cheaper model).
    #[conf(long, env, default_value = "claude-sonnet-4-5-20250929")]
    pub model: String,
    /// Maximum tokens for the compaction response.
    #[conf(long, env, default_value = "2048")]
    pub max_tokens: u32,
    /// Trigger compaction when message buffer exceeds this many characters.
    #[conf(long, env, default_value = "50000")]
    pub trigger_chars: u32,
    /// Minimum interval between automatic compactions (not user-requested).
    /// If omitted, AI compaction always runs (no rate limiting).
    /// If set, compaction is rate-limited and low-priority messages are dropped instead.
    #[conf(long, env, value_parser = conf_extra::parse_duration, serde(use_value_parser))]
    pub min_interval: Option<Duration>,
}

/// Error type for Claude API operations.
#[derive(Debug, thiserror::Error)]
pub enum ClaudeError {
    /// Failed to read API key file.
    #[error("failed to read API key file: {0}")]
    ApiKeyRead(std::io::Error),
    /// Failed to read system prompt file.
    #[error("failed to read system prompt file {0:?}: {1}")]
    SystemPromptRead(PathBuf, std::io::Error),
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
}

/// Claude API assistant implementation.
pub struct ClaudeAssistant {
    config: ClaudeConfig,
    client: reqwest::Client,
    api_key: String,
    system_prompts: Vec<String>,
    compaction_prompt: String,
    /// Summary of previous conversation history, wrapped in XML tags.
    summary: String,
    messages: MessageBuffer,
    tool_executor: Weak<dyn ToolExecutor>,
    /// Timestamp of the last automatic compaction (not user-requested).
    last_auto_compaction: Option<std::time::Instant>,
}

impl ClaudeAssistant {
    /// Create a new Claude assistant from configuration.
    ///
    /// Reads the API key, system prompts, and compaction prompt from the configured files.
    pub fn new(
        config: ClaudeConfig,
        tool_executor: Weak<dyn ToolExecutor>,
    ) -> Result<Self, ClaudeError> {
        let api_key = std::fs::read_to_string(&config.api_key_file)
            .map_err(ClaudeError::ApiKeyRead)?
            .trim()
            .to_string();

        let system_prompts: Result<Vec<String>, ClaudeError> = config
            .system_prompt_files
            .iter()
            .map(|path| {
                std::fs::read_to_string(path)
                    .map_err(|e| ClaudeError::SystemPromptRead(path.clone(), e))
            })
            .collect();
        let system_prompts = system_prompts?;

        let compaction_prompt = std::fs::read_to_string(&config.compaction.prompt_file)
            .map_err(|e| ClaudeError::SystemPromptRead(config.compaction.prompt_file.clone(), e))?;

        Ok(Self {
            config,
            client: reqwest::Client::new(),
            api_key,
            system_prompts,
            compaction_prompt,
            summary: String::new(),
            messages: MessageBuffer::new(),
            tool_executor,
            last_auto_compaction: None,
        })
    }

    /// Check if automatic compaction should be triggered and handle it.
    async fn maybe_compact(&mut self) {
        let buffer_chars = self.messages.total_chars();
        if buffer_chars <= self.config.compaction.trigger_chars as usize {
            return;
        }

        // Check if rate limiting is configured
        let can_compact = match self.config.compaction.min_interval {
            None => true,
            Some(min_interval) => self
                .last_auto_compaction
                .map(|t| t.elapsed() >= min_interval)
                .unwrap_or(true),
        };

        if can_compact {
            self.do_compact(true, None).await;
        } else {
            self.drop_oldest_messages();
        }
    }

    /// Drop oldest messages until buffer is under the trigger threshold.
    fn drop_oldest_messages(&mut self) {
        let target = self.config.compaction.trigger_chars as usize;
        let before_chars = self.messages.total_chars();

        if before_chars <= target {
            return;
        }

        let mut dropped = 0;
        while self.messages.total_chars() > target && !self.messages.is_empty() {
            self.messages.pop_front();
            dropped += 1;
        }

        let after_chars = self.messages.total_chars();
        warn!(
            "Compaction rate-limited: dropped {} oldest messages ({} -> {} chars)",
            dropped, before_chars, after_chars
        );
    }

    /// Perform compaction by summarizing messages and storing the result.
    ///
    /// If `cancel` is provided, the operation can be interrupted.
    async fn do_compact(&mut self, is_automatic: bool, cancel: Option<&CancellationToken>) {
        if self.messages.is_empty() {
            return;
        }

        if is_automatic {
            self.last_auto_compaction = Some(std::time::Instant::now());
        }

        let num_messages = self.messages.len();
        let buffer_chars = self.messages.total_chars();
        warn!(
            "Starting {} compaction: {} messages, {} chars",
            if is_automatic { "automatic" } else { "manual" },
            num_messages,
            buffer_chars
        );

        // Build system content: compaction prompt first, then other system prompts, then existing summary
        let mut system: Vec<SystemContent> = Vec::new();
        system.push(SystemContent::text(&self.compaction_prompt));
        for prompt in &self.system_prompts {
            system.push(SystemContent::text(prompt));
        }
        if !self.summary.is_empty() {
            system.push(SystemContent::text(&self.summary));
        }
        if self.config.prompt_caching
            && let Some(last) = system.last_mut()
        {
            last.set_cached();
        }

        let messages = self.messages.make_contiguous();
        let request_body = MessagesRequest {
            model: &self.config.compaction.model,
            max_tokens: self.config.compaction.max_tokens,
            system: &system,
            messages,
            tools: Vec::new(),
        };

        let http_fut = self
            .client
            .post(&self.config.claude_api_url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_API_VERSION)
            .header("content-type", "application/json")
            .json(&request_body)
            .send();

        let result = if let Some(cancel) = cancel {
            tokio::select! {
                result = http_fut => result,
                _ = cancel.cancelled() => {
                    info!("Compaction cancelled");
                    return;
                }
            }
        } else {
            http_fut.await
        };

        match result {
            Ok(response) if response.status().is_success() => {
                match response.json::<MessagesResponse>().await {
                    Ok(parsed) => {
                        let summary_text: String = parsed
                            .content
                            .into_iter()
                            .filter_map(|block| {
                                if let ContentBlock::Text { text } = block {
                                    Some(String::from(text))
                                } else {
                                    None
                                }
                            })
                            .collect::<Vec<_>>()
                            .join("\n");

                        self.summary =
                            format!("<summary type=\"activity\">\n{}\n</summary>", summary_text);
                        self.messages.clear();

                        info!(
                            "Compaction complete: summarized {} messages into {} chars",
                            num_messages,
                            self.summary.len()
                        );
                    }
                    Err(e) => {
                        error!("Compaction failed to parse response: {}", e);
                    }
                }
            }
            Ok(response) => {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                error!("Compaction API error ({}): {}", status, body);
            }
            Err(e) => {
                error!("Compaction request failed: {}", e);
            }
        }
    }

    /// Handle a single request to the Claude API with tool use loop.
    ///
    /// Returns `Ok(None)` if the operation was cancelled.
    async fn handle_request(
        &mut self,
        cancel: &CancellationToken,
    ) -> Result<Option<AssistantResponse>, ClaudeError> {
        let max_iterations = self.config.claude_max_iterations;

        let executor = self.tool_executor.upgrade();
        let tools = executor.as_ref().map(|te| te.tools()).unwrap_or_default();

        let mut attachments: Vec<PathBuf> = Vec::new();

        if let Some(last) = self.messages.last()
            && let Some(ContentBlock::Text { text, .. }) = last.content.first()
        {
            info!("Claude request: {}", text);
        }

        // Build system content blocks
        let mut system: Vec<SystemContent> = self
            .system_prompts
            .iter()
            .map(SystemContent::text)
            .collect();
        if !self.summary.is_empty() {
            system.push(SystemContent::text(&self.summary));
        }
        if self.config.prompt_caching
            && let Some(last) = system.last_mut()
        {
            last.set_cached();
        }

        for iteration in 0..max_iterations {
            if cancel.is_cancelled() {
                return Ok(None);
            }

            let messages = self.messages.make_contiguous();
            let request_body = MessagesRequest {
                model: &self.config.claude_model,
                max_tokens: self.config.claude_max_tokens,
                system: &system,
                messages,
                tools: tools.clone(),
            };

            let response = tokio::select! {
                result = self
                    .client
                    .post(&self.config.claude_api_url)
                    .header("x-api-key", &self.api_key)
                    .header("anthropic-version", ANTHROPIC_API_VERSION)
                    .header("content-type", "application/json")
                    .json(&request_body)
                    .send() => result?,
                _ = cancel.cancelled() => return Ok(None),
            };

            if !response.status().is_success() {
                let error: ErrorResponse = response.json().await?;
                return Err(ClaudeError::ApiError(error.error.message));
            }

            let response: MessagesResponse = response.json().await?;
            info!(
                "Claude response (stop_reason={}): {:?}",
                response.stop_reason, response.content
            );

            if response.stop_reason.as_ref() == "tool_use" {
                let Some(executor) = self.tool_executor.upgrade() else {
                    return Err(ClaudeError::ToolExecutorGone);
                };

                self.messages
                    .push(MessageContent::assistant(response.content.clone()));

                for block in &response.content {
                    if let ContentBlock::ToolUse { id, name, input } = block {
                        if cancel.is_cancelled() {
                            return Ok(None);
                        }

                        info!("Claude tool use: {}({})", name, input);
                        let tool_result = tokio::select! {
                            result = executor.execute(name, input) => result,
                            _ = cancel.cancelled() => return Ok(None),
                        };
                        let (result_text, is_error) = match tool_result {
                            Ok(tool_result) => {
                                info!("Tool result: {}", tool_result.text);
                                attachments.extend(tool_result.attachments);
                                (tool_result.text, false)
                            }
                            Err(err) => {
                                info!("Tool error: {}", err);
                                (err, true)
                            }
                        };
                        self.messages.push(MessageContent::tool_result(
                            id.to_string(),
                            result_text,
                            is_error,
                        ));
                    }
                }

                info!("Tool use iteration {}, continuing...", iteration + 1);
                continue;
            }

            // No tool use, extract final text response
            let text: String = response
                .content
                .into_iter()
                .filter_map(|block| {
                    if let ContentBlock::Text { text, .. } = block {
                        Some(text.into_string())
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");

            info!("Claude final result: {}", text);
            return Ok(Some(AssistantResponse::with_attachments(text, attachments)));
        }

        Err(ClaudeError::TooManyIterations(max_iterations))
    }
}

#[async_trait::async_trait]
impl Assistant for ClaudeAssistant {
    async fn record_message(&mut self, message: ChatMessage) {
        let mc = message_to_content(message);
        self.messages.push(mc);
        self.maybe_compact().await;
    }

    async fn prompt(
        &mut self,
        message: ChatMessage,
        cancel: CancellationToken,
    ) -> Result<Option<AssistantResponse>, Box<dyn std::error::Error + Send + Sync>> {
        let mc = message_to_content(message);
        self.messages.push(mc);
        self.maybe_compact().await;

        if cancel.is_cancelled() {
            return Ok(None);
        }

        Ok(self.handle_request(&cancel).await?)
    }

    async fn compact(&mut self, cancel: CancellationToken) {
        // Manual compaction always runs (is_automatic = false)
        self.do_compact(false, Some(&cancel)).await;
    }

    fn debug_log(&mut self) {
        if self.summary.is_empty() {
            info!("Claude summary: (empty)");
        } else {
            info!("Claude summary:\n{}", self.summary);
        }
        info!("Claude message buffer:\n{:#?}", self.messages);
    }
}

/// Convert a ChatMessage to a MessageContent for the API.
fn message_to_content(msg: ChatMessage) -> MessageContent {
    let ts = msg.timestamp.format("%Y-%m-%dT%H:%M:%S%.3fZ");
    let text = match msg.sent_by.prefix() {
        Some(prefix) => format!("[{}] {} {}", ts, prefix, msg.text),
        None => format!("[{}] {}", ts, msg.text),
    };
    MessageContent {
        role: msg.sent_by.api_role().into(),
        content: vec![ContentBlock::Text { text: text.into() }],
    }
}
