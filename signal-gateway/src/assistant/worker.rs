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
    // Serialize input
    input_rx: mpsc::Receiver<Input>,
    // Used when the user wants to cancel the current requests, but not shutdown the service
    stop_rx: mpsc::Receiver<()>,
    // Used when the user wants to shut down the service
    cancellation_token: CancellationToken,
}

impl AssistantWorker {
    /// Create a new assistant worker.
    pub fn new(
        assistant: Box<dyn Assistant>,
        input_rx: mpsc::Receiver<Input>,
        stop_rx: mpsc::Receiver<()>,
        cancellation_token: CancellationToken,
    ) -> Self {
        Self {
            assistant,
            input_rx,
            stop_rx,
            cancellation_token,
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
                    let was_canceled = self.handle_input(input).await;
                    if was_canceled {
                        // If the overall cancellation token was canceled,
                        // then don't even bother calling handle_stop, just exit
                        if self.cancellation_token.is_cancelled() {
                            return;
                        }
                        self.handle_stop().await;
                    }
                }
                _ = self.stop_rx.recv() => {
                    self.handle_stop().await;
                }
                _ = self.cancellation_token.cancelled() => {
                    return;
                }
            }
        }
    }

    async fn handle_input(&mut self, input: Input) -> bool {
        match input {
            Input::Prompt(msg, sender) => {
                // Make a child cancel token for each new request
                let cancel_token = self.cancellation_token.child_token();

                let mut assistant_fut = self.assistant.prompt(msg, cancel_token.clone());
                let mut was_canceled = false;

                // If the assistant finishes normally, return its result.
                // If we get a stop request, cancel the token, then wait for assistant to finish.
                let result = tokio::select! {
                    result = &mut assistant_fut => {
                        result
                    },
                    _ = self.stop_rx.recv() => {
                        cancel_token.cancel();
                        was_canceled = true;
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

                was_canceled
            }
            Input::Record(msg) => {
                self.assistant.record_message(msg).await;
                false
            }
            Input::Compact => {
                let cancel_token = self.cancellation_token.child_token();
                let mut compact_fut = self.assistant.compact(cancel_token.clone());

                tokio::select! {
                    _ = &mut compact_fut => {},
                    _ = self.stop_rx.recv() => {
                        cancel_token.cancel();
                        compact_fut.await;
                        return true;
                    }
                };
                false
            }
            Input::Debug => {
                self.assistant.debug_log();
                false
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
