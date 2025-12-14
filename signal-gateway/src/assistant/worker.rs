//! Background worker that processes assistant requests serially.

use super::AssistantError;
use crate::message_handler::AdminMessageResponse;
use signal_gateway_assistant::{Assistant, AssistantResponse, ChatMessage};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use tracing::error;

/// Result sender for prompt requests.
pub type ResultSender = oneshot::Sender<Result<AdminMessageResponse, AssistantError>>;

/// Input messages for the worker.
pub enum Input {
    /// A prompt that expects a response.
    Prompt(ChatMessage, ResultSender),
    /// A message to record without expecting a response.
    Record(ChatMessage),
    /// Request to compact the message history.
    Compact,
    /// Request to log debug info.
    Debug,
}

/// Background worker that processes assistant requests serially.
pub struct AssistantWorker {
    assistant: Box<dyn Assistant>,
    input_rx: mpsc::Receiver<Input>,
    stop_rx: mpsc::Receiver<()>,
}

impl AssistantWorker {
    /// Create a new assistant worker.
    pub fn new(
        assistant: Box<dyn Assistant>,
        input_rx: mpsc::Receiver<Input>,
        stop_rx: mpsc::Receiver<()>,
    ) -> Self {
        Self {
            assistant,
            input_rx,
            stop_rx,
        }
    }

    /// Run the worker loop, processing requests serially.
    pub async fn run(mut self) {
        loop {
            tokio::select! {
                input = self.input_rx.recv() => {
                    let Some(input) = input else {
                        break; // Channel closed
                    };
                    self.handle_input(input).await;
                }
                _ = self.stop_rx.recv() => {
                    self.handle_stop().await;
                }
            }
        }
    }

    async fn handle_input(&mut self, input: Input) {
        match input {
            Input::Prompt(msg, sender) => {
                // Make a cancel token for each new request
                let cancel_token = CancellationToken::new();

                let mut assistant_fut = self.assistant.prompt(msg, cancel_token.clone());

                // If the assistant finishes normally, return its result.
                // If we get a stop request, cancel the token, then wait for assistant to finish.
                let result = tokio::select! {
                    result = &mut assistant_fut => {
                        result
                    },
                    _ = self.stop_rx.recv() => {
                        cancel_token.cancel();
                        assistant_fut.await
                    }
                };

                let response = match result {
                    Ok(Some(resp)) => Ok(assistant_response_to_admin(resp)),
                    Ok(None) => Err(AssistantError::Cancelled),
                    Err(e) => {
                        error!("Assistant error: {}", e);
                        // Return the error message as the response text
                        Ok(AdminMessageResponse::new(format!("Error: {}", e)))
                    }
                };

                let _ = sender.send(response);
            }
            Input::Record(msg) => {
                self.assistant.record_message(msg).await;
            }
            Input::Compact => {
                self.assistant.compact().await;
            }
            Input::Debug => {
                self.assistant.debug_log();
            }
        }
    }

    async fn handle_stop(&mut self) {
        // Drain remaining stop signals
        while self.stop_rx.try_recv().is_ok() {}

        // Drain pending inputs, recording messages but cancelling prompts
        while let Ok(input) = self.input_rx.try_recv() {
            match input {
                Input::Prompt(msg, sender) => {
                    // Record the message even though we're cancelling
                    self.assistant.record_message(msg).await;
                    let _ = sender.send(Err(AssistantError::Cancelled));
                }
                Input::Record(msg) => {
                    self.assistant.record_message(msg).await;
                }
                Input::Compact | Input::Debug => {}
            }
        }
    }
}

/// Convert an AssistantResponse to an AdminMessageResponse.
fn assistant_response_to_admin(resp: AssistantResponse) -> AdminMessageResponse {
    AdminMessageResponse::new(resp.text).with_attachments(resp.attachments)
}
