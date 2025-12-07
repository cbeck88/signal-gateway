//! Tool definitions and executor trait for the Claude API.

use async_trait::async_trait;
use serde::Serialize;
use serde_json::Value;

/// Trait for executing tools. Implement this to provide tool capabilities.
#[async_trait]
pub trait ToolExecutor: Send + Sync {
    /// Get the list of available tools as JSON tool definitions.
    fn tools(&self) -> Vec<Tool>;

    /// Check if this executor handles a tool with the given name.
    fn has_tool(&self, name: &str) -> bool {
        self.tools().iter().any(|t| t.name == name)
    }

    /// Execute a tool by name with the given input arguments.
    /// Returns the result as a string to be sent back to Claude.
    async fn execute(&self, name: &str, input: &Value) -> Result<String, String>;
}

/// A tool definition for the Claude API.
#[derive(Clone, Debug, Serialize)]
pub struct Tool {
    /// The name of the tool.
    pub name: &'static str,
    /// A description of what the tool does.
    pub description: &'static str,
    /// JSON schema for the tool's input parameters.
    pub input_schema: Value,
}
