//! Provider and tool contracts.
//!
//! Defines the abstractions that higher layers (ri-ai, ri-tools) implement:
//! model metadata, the LLM provider trait, the tool trait, request/error
//! types, and auth flow descriptions.

use std::path::PathBuf;
use std::pin::Pin;
use async_trait::async_trait;
use futures::Stream;
use serde::{Deserialize, Serialize};

use crate::model::{Message, SessionId};
use crate::stream::StreamEvent;

// -- Model --

/// An LLM model with its capabilities and pricing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Model {
    pub id: String,
    pub name: String,
    pub reasoning: bool,
    pub context_window: usize,
    pub max_tokens: usize,
    pub cost: ModelCost,
}

/// Per-million-token pricing for a model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelCost {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingLevel {
    Off,
    Low,
    Medium,
    High,
    #[serde(rename = "xhigh")]
    XHigh,
}

// -- Tools --

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

/// Ambient context passed to every tool invocation by the agent loop.
/// Carries the working directory and the identity of the calling session
/// so that meta-tools (e.g. runAgent) can establish parent relationships.
#[derive(Debug, Clone)]
pub struct ToolContext {
    pub cwd: PathBuf,
    /// File-stem ID of the session that invoked this tool, if known.
    pub session_id: Option<SessionId>,
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

// -- Provider --

/// Provider-agnostic options for a single LLM request.
pub struct RequestOptions {
    pub model: Model,
    pub system_prompt: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSchema>,
    pub thinking: ThinkingLevel,
    pub max_tokens: Option<usize>,
    /// Whether to enable all of a model's built-in capabilities (e.g. Gemini's google_search + code_execution).
    pub native_tools: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("HTTP error: {0}")]
    Http(String),

    #[error("API error ({status}): {message}")]
    Api { status: u16, message: String },

    #[error("Context overflow: used {used} of {limit} tokens")]
    ContextOverflow { used: usize, limit: usize },

    #[error("Rate limited, retry after {retry_after_ms}ms")]
    RateLimited { retry_after_ms: u64 },

    #[error("Stream parse error: {0}")]
    StreamParse(String),

    #[error("{0}")]
    Other(String),
}

pub type EventStream = Pin<Box<dyn Stream<Item = Result<StreamEvent, ApiError>> + Send>>;

/// How a provider authenticates. The CLI interprets this
/// and drives the appropriate interactive flow.
pub enum AuthMethod {
    /// User visits URL, pastes back a code (e.g. Anthropic OAuth).
    PasteCode { url: String },
    /// CLI starts a local HTTP server, user visits URL via browser (e.g. Google OAuth).
    LocalCallback { url: String, port: u16, path: String },
    /// User types a value into a text field (e.g. API key).
    TextInput { prompt: String, placeholder: String },
}

/// The trait that LLM providers implement. Used as `dyn LlmProvider`
/// (hence `#[async_trait]` for object safety).
#[async_trait]
pub trait LlmProvider: Send + Sync {
    fn id(&self) -> &str;
    fn name(&self) -> &str;
    fn models(&self) -> Vec<Model>;
    fn is_authenticated(&self) -> bool;

    /// Human-readable label for the authenticated account (e.g. email).
    /// Returns None when not authenticated or when the provider has no
    /// account identity (like API-key-only providers).
    fn account_label(&self) -> Option<String> { None }

    /// Whether stored credentials can be removed. False when auth comes
    /// from an environment variable rather than a credential file.
    fn can_logout(&self) -> bool;

    /// Start a login flow. Returns an AuthMethod describing what
    /// the user needs to do, or None if login is not supported.
    async fn begin_login(&self) -> eyre::Result<Option<AuthMethod>>;

    /// Complete a login flow with the code/callback from the user.
    async fn complete_login(&self, response: &str) -> eyre::Result<()>;

    /// Remove stored credentials and clear in-memory auth state.
    async fn logout(&self) -> eyre::Result<()>;

    /// Stream a response from the LLM.
    async fn stream(&self, opts: RequestOptions) -> Result<EventStream, ApiError>;
}
