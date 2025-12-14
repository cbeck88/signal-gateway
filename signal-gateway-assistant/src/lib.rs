//! Assistant API used by signal-gateway.
//!
//! This crate provides abstract types for LLM assistant interactions,
//! making it easy to swap in different LLM implementations.

mod assistant;
mod chat_message;
mod response;
mod tools;

pub use assistant::Assistant;
pub use chat_message::{ChatMessage, SentBy};
pub use response::AssistantResponse;
pub use tools::{Tool, ToolExecutor, ToolResult};
