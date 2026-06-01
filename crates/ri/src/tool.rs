//! Tool execution contract.
//!
//! Defines the trait and data types for tools that agents can invoke:
//! schema (what the LLM sees), output (what flows back), and the async
//! execution trait itself. Tools capture their own ambient state in their
//! struct -- ri-core defines the contract, not the context.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::model::ContentBlock;

/// Tool schema sent to the LLM API so it knows what tools are available.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSchema {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

/// Result of executing a tool.
///
/// The content is `Vec<ContentBlock>` -- the same shape `ToolResult.content`
/// already has -- so a tool can return text, a binary `Blob`, or a mix, and
/// the agent-loop conversion to a `ToolResult` is a straight move with no
/// flatten.
pub struct ToolOutput {
    /// What flows back to the LLM as the tool result.
    pub content: Vec<ContentBlock>,
    pub is_error: bool,
    /// Structured data for UI rendering. Not sent to the LLM.
    pub details: Option<serde_json::Value>,
}

impl ToolOutput {
    /// A plain-text result -- the overwhelmingly common case.
    pub fn text(s: impl Into<String>) -> Self {
        Self { content: vec![ContentBlock::text(s)], is_error: false, details: None }
    }

    /// An error result. Signature unchanged from the pre-reshape API.
    pub fn error(s: impl Into<String>) -> Self {
        Self { content: vec![ContentBlock::text(s)], is_error: true, details: None }
    }

    /// A result carrying arbitrary content blocks (e.g. a `Blob`).
    pub fn blocks(content: Vec<ContentBlock>) -> Self {
        Self { content, is_error: false, details: None }
    }

    /// Attach structured UI details.
    pub fn with_details(mut self, details: serde_json::Value) -> Self {
        self.details = Some(details);
        self
    }

    /// Set the error flag.
    pub fn with_error(mut self, is_error: bool) -> Self {
        self.is_error = is_error;
        self
    }

    /// Concatenate the text of every `Text` block (joined by newlines) for
    /// callers that still want a flat string -- logging, `tag_host` -- without
    /// breaking on a `Blob`-only output.
    pub fn text_str(&self) -> String {
        self.content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
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
