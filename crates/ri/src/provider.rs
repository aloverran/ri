//! Provider interface, model definitions, tools, and stream events.
//!
//! Defined in the core `ri` crate so the agent loop can call providers
//! without depending on `ri-ai`. Contains everything needed to define
//! and interact with LLM providers: model metadata, the streaming event
//! protocol, tool definitions, and the LlmProvider trait itself.

use std::path::PathBuf;
use std::pin::Pin;
use std::future::Future;
use async_trait::async_trait;
use futures::Stream;
use serde::{Deserialize, Serialize};

use crate::message::{Message, Usage};

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

// -- Stream events --

/// Normalized stream events emitted by LLM providers during response streaming.
/// The agent loop consumes these to accumulate content blocks and detect tool calls.
#[derive(Debug, Clone)]
pub enum StreamEvent {
    TextStart,
    TextDelta(String),
    TextEnd { sig: Option<String> },
    ThinkingStart,
    ThinkingDelta(String),
    ThinkingEnd { sig: Option<String> },
    ToolCallStart { id: String, name: String },
    ToolCallDelta { id: String, json_fragment: String },
    ToolCallEnd { id: String, sig: Option<String> },
    Usage(Usage),
    Done,
    Error(String),
}

// -- Tools --

/// Tool schema sent to the LLM API so it knows what tools are available.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSchema {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

/// Result of executing a tool. Text is returned to the LLM as a tool_result content block.
pub struct ToolOutput {
    pub text: String,
    pub is_error: bool,
}

/// Function pointer type for tool implementations. Each tool is a plain fn that
/// returns a boxed future (needed to store heterogeneous async fns in a Vec).
pub type ToolFn = fn(
    serde_json::Value,
    PathBuf,
    tokio_util::sync::CancellationToken,
) -> Pin<Box<dyn Future<Output = ToolOutput> + Send>>;

/// Static tool definition: schema for the LLM + implementation function.
pub struct ToolDef {
    pub name: &'static str,
    pub description: &'static str,
    pub parameters: serde_json::Value,
    pub run: ToolFn,
}

impl ToolDef {
    pub fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.name.to_string(),
            description: self.description.to_string(),
            parameters: self.parameters.clone(),
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
}

/// The trait that LLM providers implement. Used as `dyn LlmProvider`
/// (hence `#[async_trait]` for object safety).
#[async_trait]
pub trait LlmProvider: Send + Sync {
    fn id(&self) -> &str;
    fn name(&self) -> &str;
    fn models(&self) -> Vec<Model>;
    fn is_authenticated(&self) -> bool;

    /// Start a login flow. Returns an AuthMethod describing what
    /// the user needs to do, or None if login is not supported.
    async fn begin_login(&self) -> eyre::Result<Option<AuthMethod>>;

    /// Complete a login flow with the code/callback from the user.
    async fn complete_login(&self, response: &str) -> eyre::Result<()>;

    /// Stream a response from the LLM.
    async fn stream(&self, opts: RequestOptions) -> Result<EventStream, ApiError>;
}
