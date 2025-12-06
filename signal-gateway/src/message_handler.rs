//! Message handler types for admin messages not handled by the gateway.

use async_trait::async_trait;
use std::{error::Error, path::PathBuf};

/// A verified Signal message from an admin.
///
/// This struct contains the message content and metadata for a message
/// that has been verified as coming from a trusted admin.
#[non_exhaustive]
#[derive(Clone, Debug)]
pub struct AdminMessage {
    /// The text content of the message.
    pub message: String,
    /// The timestamp of the message (milliseconds since Unix epoch).
    pub timestamp: u64,
    /// The UUID of the sender.
    pub sender_uuid: String,
    /// The group ID if this message was sent to a group.
    pub group_id: Option<String>,
}

/// Context for message handler operations.
///
/// This trait provides access to gateway functionality that message handlers
/// may need. Currently empty, but reserved for future expansion.
#[async_trait]
pub trait Context: Send + Sync {}

/// Response to an admin message.
#[non_exhaustive]
#[derive(Clone, Debug, Default)]
pub struct AdminMessageResponse {
    /// The text response to send back to the admin.
    pub text: String,
    /// Optional file attachments to include with the response.
    pub attachments: Vec<PathBuf>,
}

impl AdminMessageResponse {
    /// Create a new response with the given text.
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            attachments: Vec::new(),
        }
    }

    /// Create a builder for constructing a response.
    pub fn builder() -> AdminMessageResponseBuilder {
        AdminMessageResponseBuilder::default()
    }

    /// Add an attachment to the response.
    pub fn with_attachment(mut self, path: impl Into<PathBuf>) -> Self {
        self.attachments.push(path.into());
        self
    }

    /// Add multiple attachments to the response.
    pub fn with_attachments(mut self, paths: impl IntoIterator<Item = impl Into<PathBuf>>) -> Self {
        self.attachments.extend(paths.into_iter().map(Into::into));
        self
    }
}

/// Builder for constructing an [`AdminMessageResponse`].
#[derive(Clone, Debug, Default)]
pub struct AdminMessageResponseBuilder {
    text: String,
    attachments: Vec<PathBuf>,
}

impl AdminMessageResponseBuilder {
    /// Set the text response.
    pub fn text(mut self, text: impl Into<String>) -> Self {
        self.text = text.into();
        self
    }

    /// Add an attachment.
    pub fn attachment(mut self, path: impl Into<PathBuf>) -> Self {
        self.attachments.push(path.into());
        self
    }

    /// Add multiple attachments.
    pub fn attachments(mut self, paths: impl IntoIterator<Item = impl Into<PathBuf>>) -> Self {
        self.attachments.extend(paths.into_iter().map(Into::into));
        self
    }

    /// Build the response.
    pub fn build(self) -> AdminMessageResponse {
        AdminMessageResponse {
            text: self.text,
            attachments: self.attachments,
        }
    }
}

/// Result type for message handler responses.
pub type MessageHandlerResult = Result<AdminMessageResponse, (u16, Box<dyn Error + Send + Sync>)>;

/// Handler for admin messages that don't start with `/`.
#[async_trait]
pub trait MessageHandler: Send + Sync {
    /// Handle a verified Signal message from an admin.
    ///
    /// This is called for admin messages that don't start with `/` (which are
    /// handled as gateway commands).
    async fn handle_verified_signal_message(
        &self,
        msg: AdminMessage,
        context: &dyn Context,
    ) -> MessageHandlerResult;
}
