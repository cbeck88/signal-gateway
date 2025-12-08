//! Background worker that processes Claude API requests serially.

use super::{
    ClaudeConfig, ClaudeError, ContentBlock, ErrorResponse, MessageContent, MessagesRequest,
    MessagesResponse, ToolExecutor, ANTHROPIC_API_VERSION,
};
use std::sync::Weak;
use tokio::sync::{mpsc, oneshot};
use tracing::info;

/// A request to be processed by the Claude worker.
pub struct ClaudeRequest {
    pub prompt: String,
    pub result_sender: oneshot::Sender<Result<String, String>>,
}

/// Background worker that processes Claude API requests serially.
pub struct ClaudeWorker {
    config: ClaudeConfig,
    client: reqwest::Client,
    api_key: String,
    system_prompt: String,
    tool_executor: Weak<dyn ToolExecutor>,
    request_rx: mpsc::Receiver<ClaudeRequest>,
}

impl ClaudeWorker {
    /// Create a new Claude worker.
    ///
    /// Reads the API key and system prompt from the configured files.
    pub fn new(
        config: ClaudeConfig,
        tool_executor: Weak<dyn ToolExecutor>,
        request_rx: mpsc::Receiver<ClaudeRequest>,
    ) -> Result<Self, ClaudeError> {
        let api_key = std::fs::read_to_string(&config.api_key_file)
            .map_err(ClaudeError::ApiKeyRead)?
            .trim()
            .to_owned();

        let system_prompt = std::fs::read_to_string(&config.system_prompt_file)
            .map_err(ClaudeError::SystemPromptRead)?;

        Ok(Self {
            config,
            client: reqwest::Client::new(),
            api_key,
            system_prompt,
            tool_executor,
            request_rx,
        })
    }

    /// Run the worker loop, processing requests serially.
    pub async fn run(mut self) {
        while let Some(request) = self.request_rx.recv().await {
            let result = self
                .handle_request(&request.prompt)
                .await
                .map_err(|e| e.to_string());
            // Ignore send errors - the caller may have dropped the receiver
            let _ = request.result_sender.send(result);
        }
    }

    /// Handle a single request to the Claude API.
    async fn handle_request(&self, prompt: &str) -> Result<String, ClaudeError> {
        let max_iterations = self.config.claude_max_iterations;

        // Get tools from the executor if still alive
        let executor = self.tool_executor.upgrade();
        let tools = executor.as_ref().map(|te| te.tools()).unwrap_or_default();
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
