use chrono::{DateTime, Utc};

/// Indicates the "role" i.e. the manner in which a particular message was sent
#[non_exhaustive]
pub enum SentBy {
    /// User message directed at the system (commands, etc.)
    UserToSystem,
    /// User message directed at assistant (prompts)
    UserToAssistant,
    /// Response from assistant
    Assistant,
    /// System-generated message
    System,
    /// Alert from alertmanager
    AlertManager,
}

impl SentBy {
    /// Returns the API role: "assistant" for Assistant, "user" for everything else.
    pub fn api_role(&self) -> &'static str {
        match self {
            Self::Assistant => "assistant",
            _ => "user",
        }
    }

    /// Returns a prefix to prepend to message text for context.
    pub fn prefix(&self) -> Option<&'static str> {
        match self {
            Self::UserToSystem => Some("[user to system]"),
            Self::UserToAssistant => None, // No prefix needed for direct user messages
            Self::Assistant => None,
            Self::System => Some("[system]"),
            Self::AlertManager => Some("[alertmanager]"),
        }
    }
}

/// A chat message
pub struct ChatMessage {
    pub sent_by: SentBy,
    pub timestamp: DateTime<Utc>,
    pub text: Box<str>,
}
