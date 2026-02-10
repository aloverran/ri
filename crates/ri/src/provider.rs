// Provider trait -- the interface that LLM providers implement.
//
// ri-ai implements this trait. ri defines it so the agent loop
// can call providers without depending on ri-ai.

use std::pin::Pin;
use async_trait::async_trait;
use futures::Stream;

use crate::event::StreamEvent;
use crate::tool::ToolSchema;
use crate::model::{Model, ThinkingLevel};
use crate::message::Message;

// Request-level options (provider-agnostic).
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

// How a provider authenticates. The CLI interprets this
// and drives the appropriate interactive flow.
pub enum AuthMethod {
    // User visits URL, pastes back a code.
    PasteCode { url: String },
    // CLI starts a local HTTP server, user visits URL via browser.
    LocalCallback { url: String, port: u16, path: String },
}

// Info about a provider for display in login UI etc.
pub struct ProviderInfo {
    pub id: &'static str,
    pub name: &'static str,
}

// The trait that LLM providers implement.
#[async_trait]
pub trait LlmProvider: Send + Sync {
    // Identity.
    fn id(&self) -> &str;
    fn name(&self) -> &str;

    // Model catalog for this provider.
    fn models(&self) -> Vec<Model>;

    // Whether this provider currently has valid credentials.
    fn is_authenticated(&self) -> bool;

    // Start a login flow. Returns an AuthMethod describing what
    // the user needs to do, or None if login is not supported.
    fn begin_login(&self) -> eyre::Result<Option<AuthMethod>>;

    // Complete a login flow. The response is the code/key from the user.
    async fn complete_login(&self, response: &str) -> eyre::Result<()>;

    // Stream a response from the LLM.
    async fn stream(&self, opts: RequestOptions) -> Result<EventStream, ApiError>;
}
