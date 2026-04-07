//! Tool execution contract.
//!
//! Defines the trait and data types for tools that agents can invoke:
//! schema (what the LLM sees), output (what flows back), and the async
//! execution trait itself. Tools capture their own ambient state in their
//! struct -- ri-core defines the contract, not the context.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// Tool schema sent to the LLM API so it knows what tools are available.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSchema {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

/// Result of executing a tool.
pub struct ToolOutput {
    /// Text content sent to the LLM as the tool result.
    pub text: String,
    pub is_error: bool,
    /// Structured data for UI rendering. Not sent to the LLM.
    pub details: Option<serde_json::Value>,
}

impl ToolOutput {
    pub fn error(msg: impl Into<String>) -> Self {
        Self { text: msg.into(), is_error: true, details: None }
    }
}

/// A tool the agent can invoke. Provides schema (for the LLM API)
/// and an async execution function.
///
/// Each tool captures whatever ambient state it needs (cwd, SSH
/// connection, session references) in its own struct at construction
/// time. The trait itself is environment-agnostic.
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters(&self) -> serde_json::Value;

    async fn run(
        &self,
        input: serde_json::Value,
        cancel: tokio_util::sync::CancellationToken,
    ) -> ToolOutput;

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.name().to_string(),
            description: self.description().to_string(),
            parameters: self.parameters(),
        }
    }
}
