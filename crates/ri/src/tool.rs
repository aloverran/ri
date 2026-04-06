//! Tool execution contract.
//!
//! Defines the trait and data types for tools that agents can invoke:
//! schema (what the LLM sees), context (ambient state from the agent loop),
//! output (what flows back), and the async execution trait itself.

use std::collections::HashMap;
use std::path::PathBuf;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::model::SessionId;

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

/// Ambient context passed to every tool invocation by the agent loop.
/// Carries the working directory, the identity of the calling session,
/// and any extra environment variables to inject into spawned processes.
#[derive(Debug, Clone)]
pub struct ToolContext {
    pub cwd: PathBuf,
    /// File-stem ID of the session that invoked this tool, if known.
    pub session_id: Option<SessionId>,
    /// Extra environment variables injected into bash processes.
    /// Domain-agnostic: the agent loop populates this with whatever
    /// the harness needs (gatekeeper grants, API tokens, etc).
    pub env_vars: HashMap<String, String>,
}

/// A tool the agent can invoke. Provides schema (for the LLM API)
/// and an async execution function.
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters(&self) -> serde_json::Value;

    async fn run(
        &self,
        input: serde_json::Value,
        ctx: ToolContext,
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
