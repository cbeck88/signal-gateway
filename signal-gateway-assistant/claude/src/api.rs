//! Anthropic Claude API types.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use signal_gateway_assistant::Tool;

/// Request body for the Claude Messages API.
#[derive(Serialize)]
pub(crate) struct MessagesRequest<'a> {
    pub model: &'a str,
    pub max_tokens: u32,
    pub system: &'a [SystemContent],
    pub messages: &'a [MessageContent],
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<Tool>,
}

/// A content block in the system prompt array.
#[derive(Clone, Default, Serialize)]
pub(crate) struct SystemContent {
    #[serde(rename = "type")]
    content_type: &'static str,
    text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_control: Option<CacheControl>,
}

impl SystemContent {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            content_type: "text",
            text: text.into(),
            cache_control: None,
        }
    }

    pub fn set_cached(&mut self) {
        self.cache_control = Some(CacheControl::ephemeral());
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
pub(crate) struct MessageContent {
    pub role: Box<str>,
    pub content: Vec<ContentBlock>,
}

impl MessageContent {
    pub fn assistant(blocks: Vec<ContentBlock>) -> Self {
        Self {
            role: "assistant".into(),
            content: blocks,
        }
    }

    pub fn tool_result(tool_use_id: String, content: String, is_error: bool) -> Self {
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
pub(crate) enum ContentBlock {
    Text {
        text: Box<str>,
    },
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
pub(crate) struct MessagesResponse {
    pub content: Vec<ContentBlock>,
    pub stop_reason: Box<str>,
}

/// Error response from the Claude API.
#[derive(Deserialize)]
pub(crate) struct ErrorResponse {
    pub error: ApiErrorDetail,
}

#[derive(Deserialize)]
pub(crate) struct ApiErrorDetail {
    pub message: Box<str>,
}
