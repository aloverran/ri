// LLM provider trait -- the abstraction over Anthropic, OpenAI, Google, etc.

use async_trait::async_trait;
use futures::Stream;
use std::pin::Pin;

use crate::event::AssistantStreamEvent;
use crate::types::{CompletionOptions, Message, Model};

pub type StreamOutput =
    Pin<Box<dyn Stream<Item = Result<AssistantStreamEvent, ProviderError>> + Send>>;

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

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

#[async_trait]
pub trait LlmProvider: Send + Sync {
    fn name(&self) -> &str;

    async fn stream(
        &self,
        model: &Model,
        messages: &[Message],
        options: &CompletionOptions,
        api_key: &str,
    ) -> Result<StreamOutput, ProviderError>;
}
