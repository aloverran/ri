//! Provider trait -- the interface that LLM providers implement.
//!
//! Defined in the core `ri` crate so the agent loop can call providers
//! without depending on `ri-ai`.

use std::pin::Pin;
use async_trait::async_trait;
use futures::Stream;

use crate::event::StreamEvent;
use crate::tool::ToolSchema;
use crate::model::{Model, ThinkingLevel};
use crate::message::Message;

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
    fn begin_login(&self) -> eyre::Result<Option<AuthMethod>>;

    /// Complete a login flow with the code/callback from the user.
    async fn complete_login(&self, response: &str) -> eyre::Result<()>;

    /// Stream a response from the LLM.
    async fn stream(&self, opts: RequestOptions) -> Result<EventStream, ApiError>;
}
