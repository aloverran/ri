pub mod sse;
pub mod anthropic;
pub mod gemini;
pub mod http;

use std::pin::Pin;
use futures::Stream;

use crate::types::{Model, ThinkingLevel};
use ri_store::types::Message;

// The HTTP request the provider builds. Fully visible, inspectable, loggable.
#[derive(Debug, Clone)]
pub struct ApiRequest {
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

// Normalized stream events, shared across providers.
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
    Done,
    Error(String),
}

// Tool schema as seen by the LLM API.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ToolSchema {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

// Request-level options (provider-agnostic).
pub struct RequestOptions<'a> {
    pub model: &'a Model,
    pub system_prompt: &'a str,
    pub messages: &'a [Message],
    pub tools: &'a [ToolSchema],
    pub thinking: ThinkingLevel,
    pub max_tokens: Option<usize>,
}

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
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

pub type EventStream = Pin<Box<dyn Stream<Item = Result<StreamEvent, ApiError>> + Send>>;

// The provider enum. Closed set. Adding a provider means adding a variant
// and implementing build_request + event_stream. The compiler enforces
// exhaustive handling everywhere.
#[derive(Debug, Clone)]
pub enum Provider {
    Anthropic {
        api_key: String,
    },
    Gemini {
        variant: GeminiVariant,
        token: String,
        project_id: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GeminiVariant {
    Cli,
    Antigravity,
}

impl Provider {
    // Build the HTTP request. Returns pure data -- the caller can inspect,
    // log, modify, or mock before sending.
    pub fn build_request(&self, opts: &RequestOptions) -> ApiRequest {
        match self {
            Provider::Anthropic { api_key } => anthropic::build_request(api_key, opts),
            Provider::Gemini { variant, token, project_id } => {
                gemini::build_request(*variant, token, project_id, opts)
            }
        }
    }

    // Turn a byte stream (from HTTP response) into typed events.
    // Each provider creates its own SSE parser + interpreter internally.
    pub fn event_stream(
        &self,
        bytes: Pin<Box<dyn Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Send>>,
        tools: &[ToolSchema],
    ) -> EventStream {
        match self {
            Provider::Anthropic { api_key } => {
                let is_oauth = api_key.starts_with("sk-ant-oat");
                anthropic::event_stream(bytes, tools, is_oauth)
            }
            Provider::Gemini { .. } => {
                gemini::event_stream(bytes)
            }
        }
    }

    // Convenience: build request, send it, return event stream.
    pub async fn stream(&self, opts: &RequestOptions<'_>) -> Result<EventStream, ApiError> {
        let request = self.build_request(opts);

        tracing::debug!(
            url = %request.url,
            body = %request.body,
            "API request"
        );

        let bytes = http::send(&request).await?;
        Ok(self.event_stream(bytes, opts.tools))
    }
}
