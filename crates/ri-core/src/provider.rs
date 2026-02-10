// Provider trait -- the agent loop's view of an LLM provider.
//
// ri-ai implements this trait. ri-core defines it so the agent loop
// can call providers without depending on ri-ai.

use std::pin::Pin;
use futures::Stream;

use crate::event::{StreamEvent, ToolSchema};
use crate::types::{Model, ThinkingLevel};
use ri_store::types::Message;

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

use std::future::Future;

// The trait that LLM providers implement.
pub trait LlmProvider: Send + Sync {
    fn stream(
        &self,
        opts: RequestOptions,
    ) -> Pin<Box<dyn Future<Output = Result<EventStream, ApiError>> + Send + '_>>;
}
