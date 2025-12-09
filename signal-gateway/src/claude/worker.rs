//! Background worker that processes Claude API requests serially.

use super::{ANTHROPIC_API_VERSION, ClaudeConfig, ClaudeError, Tool, ToolExecutor};
use crate::message_handler::AdminMessageResponse;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Weak;
use tokio::sync::{mpsc, oneshot};
use tracing::info;

/// Sent with inputs to claude that claude is expected to respond to. The sender
/// gives the worker a way to return the results to the caller asynchronously.
pub type ResultSender = oneshot::Sender<Result<AdminMessageResponse, ClaudeError>>;

/// Indicates the "role" i.e. the manner in which a particular message was sent
pub enum SentBy {
    /// User message directed at the system (commands, etc.)
    UserToSystem,
    /// User message directed at Claude (prompts)
    UserToClaude,
    /// Response from Claude
    Claude,
    /// System-generated message
    System,
    /// Alert from alertmanager
    AlertManager,
}

impl SentBy {
    /// Returns the API role: "assistant" for Claude, "user" for everything else.
    fn api_role(&self) -> &'static str {
        match self {
            Self::Claude => "assistant",
            _ => "user",
        }
    }

    /// Returns a prefix to prepend to message text for context.
    fn prefix(&self) -> Option<&'static str> {
        match self {
            Self::UserToSystem => Some("[user to system]"),
            Self::UserToClaude => None, // No prefix needed for direct user messages
            Self::Claude => None,
            Self::System => Some("[system]"),
            Self::AlertManager => Some("[alertmanager]"),
        }
    }
}

/// An input to the worker sent through the channel
pub enum Input {
    Chat(ChatMessage),
    Compact,
    Debug,
}

/// A chat message
pub struct ChatMessage {
    pub sent_by: SentBy,
    pub timestamp: DateTime<Utc>,
    pub text: Box<str>,
    /// Present when claude is expected to respond to the message
    pub result_sender: Option<ResultSender>,
}

impl ChatMessage {
    fn into_content_and_sender(self) -> (MessageContent, Option<ResultSender>) {
        let ts = self.timestamp.format("%Y-%m-%dT%H:%M:%S%.3fZ");
        let text = match self.sent_by.prefix() {
            Some(prefix) => format!("[{}] {} {}", ts, prefix, self.text).into(),
            None => format!("[{}] {}", ts, self.text).into(),
        };
        let mc = MessageContent {
            role: self.sent_by.api_role().into(),
            content: vec![ContentBlock::Text { text }],
        };

        (mc, self.result_sender)
    }
}

/// Background worker that processes Claude API requests serially.
pub struct ClaudeWorker {
    config: ClaudeConfig,
    client: reqwest::Client,
    api_key: String,
    system_prompts: Vec<String>,
    // FIXME: use this and append to system prompt within <summary> </summary> tags
    #[allow(dead_code)]
    summary: String,
    messages: Vec<MessageContent>,
    tool_executor: Weak<dyn ToolExecutor>,
    input_rx: mpsc::Receiver<Input>,
    stop_rx: mpsc::Receiver<()>,
}

impl ClaudeWorker {
    /// Create a new Claude worker.
    ///
    /// Reads the API key and system prompts from the configured files.
    pub fn new(
        config: ClaudeConfig,
        tool_executor: Weak<dyn ToolExecutor>,
        input_rx: mpsc::Receiver<Input>,
        stop_rx: mpsc::Receiver<()>,
    ) -> Result<Self, ClaudeError> {
        let api_key = std::fs::read_to_string(&config.api_key_file)
            .map_err(ClaudeError::ApiKeyRead)?
            .trim()
            .to_owned();

        let system_prompts: Vec<String> = config
            .system_prompt_files
            .iter()
            .map(|path| {
                std::fs::read_to_string(path)
                    .map_err(|e| ClaudeError::SystemPromptRead(path.clone(), e))
            })
            .collect::<Result<_, _>>()?;

        Ok(Self {
            config,
            client: reqwest::Client::new(),
            api_key,
            system_prompts,
            summary: String::new(),
            messages: Default::default(),
            tool_executor,
            input_rx,
            stop_rx,
        })
    }

    /// Run the worker loop, processing requests serially.
    pub async fn run(mut self) {
        loop {
            tokio::select! {
                input = self.input_rx.recv() => {
                    let Some(input) = input else {
                        // Channel closed, exit
                        break;
                    };
                    match input {
                        Input::Chat(msg) => {
                            let (mc, maybe_sender) = msg.into_content_and_sender();
                            self.messages.push(mc);
                            if let Some(sender) = maybe_sender {
                                let result = self.handle_request().await;
                                // If handle_request was interrupted by stop request, go on to drain the queues
                                if matches!(result, Err(ClaudeError::StopRequested)) {
                                    self.handle_stop();
                                }
                                // Ignore send errors - the caller may have dropped the receiver
                                let _ = sender.send(result);
                            }
                        },
                        Input::Compact => {
                            self.handle_compact().await;
                        }
                        Input::Debug => {
                            self.handle_debug();
                        }
                    }
                }
                _ = self.stop_rx.recv() => {
                    self.handle_stop();
                }
            }
        }
    }

    /// Handle a stop request by draining queues and sending errors to pending requests.
    fn handle_stop(&mut self) {
        // Drain the stop_rx queue
        while self.stop_rx.try_recv().is_ok() {}

        // Drain the input_rx queue and send StopRequested to each prompt request
        while let Ok(input) = self.input_rx.try_recv() {
            match input {
                Input::Chat(msg) => {
                    let (mc, maybe_sender) = msg.into_content_and_sender();
                    self.messages.push(mc);
                    if let Some(sender) = maybe_sender {
                        let _ = sender.send(Err(ClaudeError::StopRequested));
                    }
                }
                Input::Compact | Input::Debug => {}
            }
        }
    }

    /// Check if stop has been requested.
    fn check_stop(&mut self) -> Result<(), ClaudeError> {
        match self.stop_rx.try_recv() {
            Ok(()) => Err(ClaudeError::StopRequested),
            Err(mpsc::error::TryRecvError::Empty) => Ok(()),
            Err(mpsc::error::TryRecvError::Disconnected) => Err(ClaudeError::StopRequested),
        }
    }

    /// Perform compaction
    async fn handle_compact(&mut self) {
        // FIXME: we should actually try to summarize messages using an api request, and then store it, before tossing messages
        self.messages.clear();
    }

    /// Log the message buffer for debugging.
    fn handle_debug(&self) {
        info!("Claude message buffer:\n{:#?}", self.messages);
    }

    /// Handle a single request to the Claude API.
    async fn handle_request(&mut self) -> Result<AdminMessageResponse, ClaudeError> {
        let max_iterations = self.config.claude_max_iterations;

        // Get tools from the executor if still alive
        let executor = self.tool_executor.upgrade();
        let tools = executor.as_ref().map(|te| te.tools()).unwrap_or_default();

        // Collect attachments from tool results across all iterations
        let mut attachments: Vec<PathBuf> = Vec::new();

        if let Some(last) = self.messages.last()
            && let Some(ContentBlock::Text { text, .. }) = last.content.first()
        {
            info!("Claude request: {}", text);
        }

        // Build system content blocks, caching only the last one
        let system: Vec<SystemContent> = self
            .system_prompts
            .iter()
            .enumerate()
            .map(|(i, text)| {
                let content = SystemContent::text(text);
                if i == self.system_prompts.len() - 1 {
                    content.cached()
                } else {
                    content
                }
            })
            .collect();

        for iteration in 0..max_iterations {
            // Check for stop before making API call
            self.check_stop()?;

            let request_body = MessagesRequest {
                model: &self.config.claude_model,
                max_tokens: self.config.claude_max_tokens,
                system: &system,
                messages: &self.messages,
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
            if response.stop_reason.as_ref() == "tool_use" {
                // Try to get a strong reference to the executor
                let Some(executor) = self.tool_executor.upgrade() else {
                    return Err(ClaudeError::ToolExecutorGone);
                };

                // Add assistant's response to messages
                self.messages
                    .push(MessageContent::assistant(response.content.clone()));

                // Execute each tool use and collect results
                for block in &response.content {
                    if let ContentBlock::ToolUse { id, name, input } = block {
                        // Check for stop before each tool use
                        self.check_stop()?;

                        info!("Claude tool use: {}({})", name, input);
                        let (result_text, is_error) = match executor.execute(name, input).await {
                            Ok(tool_result) => {
                                info!("Tool result: {}", tool_result.text);
                                // Collect any attachments from the tool result
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

                // Continue the loop to get Claude's next response
                info!("Tool use iteration {}, continuing...", iteration + 1);
                continue;
            }

            // No tool use, extract final text response
            let text: String = response
                .content
                .into_iter()
                .filter_map(|block| {
                    if let ContentBlock::Text { text, .. } = block {
                        Some(text)
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");

            info!("Claude final result: {}", text);
            return Ok(AdminMessageResponse::new(text).with_attachments(attachments));
        }

        Err(ClaudeError::TooManyIterations(max_iterations))
    }
}

/// Request body for the Claude Messages API.
#[derive(Serialize)]
struct MessagesRequest<'a> {
    model: &'a str,
    max_tokens: u32,
    system: &'a [SystemContent],
    messages: &'a [MessageContent],
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<Tool>,
}

/// A content block in the system prompt array.
#[derive(Clone, Serialize)]
struct SystemContent {
    #[serde(rename = "type")]
    content_type: &'static str,
    text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_control: Option<CacheControl>,
}

impl SystemContent {
    fn text(text: impl Into<String>) -> Self {
        Self {
            content_type: "text",
            text: text.into(),
            cache_control: None,
        }
    }

    fn cached(mut self) -> Self {
        self.cache_control = Some(CacheControl::ephemeral());
        self
    }
}

/// Cache control directive for prompt caching.
#[derive(Clone, Serialize)]
struct CacheControl {
    #[serde(rename = "type")]
    cache_type: &'static str,
}

impl CacheControl {
    fn ephemeral() -> Self {
        Self {
            cache_type: "ephemeral",
        }
    }
}

/// A message in the conversation (can have multiple content blocks).
#[derive(Clone, Debug, Serialize, Deserialize)]
struct MessageContent {
    role: Box<str>,
    content: Vec<ContentBlock>,
}

impl MessageContent {
    fn assistant(blocks: Vec<ContentBlock>) -> Self {
        Self {
            role: "assistant".into(),
            content: blocks,
        }
    }

    fn tool_result(tool_use_id: String, content: String, is_error: bool) -> Self {
        Self {
            role: "user".into(),
            content: vec![ContentBlock::ToolResult {
                tool_use_id: tool_use_id.into(),
                content: content.into(),
                is_error: if is_error { Some(true) } else { None },
            }],
        }
    }
}

/// A content block in the request/response.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ContentBlock {
    Text { text: Box<str> },
    ToolUse {
        id: Box<str>,
        name: Box<str>,
        input: Value,
    },
    ToolResult {
        tool_use_id: Box<str>,
        content: Box<str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
    },
}

/// Response from the Claude Messages API.
#[derive(Debug, Deserialize)]
struct MessagesResponse {
    content: Vec<ContentBlock>,
    stop_reason: Box<str>,
}

/// Error response from the Claude API.
#[derive(Deserialize)]
struct ErrorResponse {
    error: ApiErrorDetail,
}

#[derive(Deserialize)]
struct ApiErrorDetail {
    message: Box<str>,
}
