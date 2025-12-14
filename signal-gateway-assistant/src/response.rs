//! Response type for assistant interactions.

use std::path::PathBuf;

/// Response from an assistant interaction.
#[derive(Clone, Debug, Default)]
#[non_exhaustive]
pub struct AssistantResponse {
    /// Text response from the assistant.
    pub text: String,
    /// Optional file attachments generated during the interaction.
    pub attachments: Vec<PathBuf>,
}

impl AssistantResponse {
    /// Create a new response with just text.
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            attachments: Vec::new(),
        }
    }

    /// Create a response with text and attachments.
    pub fn with_attachments(text: impl Into<String>, attachments: Vec<PathBuf>) -> Self {
        Self {
            text: text.into(),
            attachments,
        }
    }

    /// Add attachments to this response.
    pub fn add_attachments(mut self, attachments: Vec<PathBuf>) -> Self {
        self.attachments.extend(attachments);
        self
    }
}

impl From<String> for AssistantResponse {
    fn from(text: String) -> Self {
        Self::new(text)
    }
}

impl From<&str> for AssistantResponse {
    fn from(text: &str) -> Self {
        Self::new(text)
    }
}
