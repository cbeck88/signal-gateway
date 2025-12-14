//! Assistant API used by signal-gateway.
//!
//! Implement the `Assistant` trait to swap in your own LLM, context management, etc.
//!
//! The assistant trait is unopinionated and you should be able to use something like
//! `rsllm` or `rig` with relative ease if you want to.

mod assistant;
mod chat_message;
mod response;
mod tools;

pub use assistant::Assistant;
pub use chat_message::{ChatMessage, SentBy};
pub use response::AssistantResponse;
pub use tools::{Tool, ToolExecutor, ToolResult};
