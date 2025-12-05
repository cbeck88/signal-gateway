//! Message handler types for admin messages not handled by the gateway.

use std::{error::Error, future::Future, path::PathBuf, pin::Pin};

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

/// Handler function for admin messages that don't start with `/`.
/// Takes the message text and returns a response.
pub type MessageHandler =
    Box<dyn Fn(String) -> Pin<Box<dyn Future<Output = MessageHandlerResult> + Send>> + Send + Sync>;
