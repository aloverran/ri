pub mod sse;
pub mod http;
pub mod anthropic;
pub mod gemini;
pub mod auth;
pub mod registry;

use std::pin::Pin;
use std::future::Future;
use futures::Stream;

use ri::{ApiError, EventStream, LlmProvider, RequestOptions, ToolSchema};

// The provider enum. Closed set.
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

// The HTTP request a provider builds. Fully visible, inspectable, loggable.
#[derive(Debug, Clone)]
pub struct ApiRequest {
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl Provider {
    pub fn is_authenticated(&self) -> bool {
        match self {
            Provider::Anthropic { api_key } => !api_key.is_empty(),
            Provider::Gemini { token, .. } => !token.is_empty(),
        }
    }

    pub fn build_request(&self, opts: &RequestOptions) -> ApiRequest {
        match self {
            Provider::Anthropic { api_key } => anthropic::build_request(api_key, opts),
            Provider::Gemini { variant, token, project_id } => {
                gemini::build_request(*variant, token, project_id, opts)
            }
        }
    }

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
}

impl LlmProvider for Provider {
    fn stream<'a>(
        &'a self,
        opts: &'a RequestOptions<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<EventStream, ApiError>> + Send + 'a>> {
        Box::pin(async move {
            let request = self.build_request(opts);

            tracing::debug!(
                url = %request.url,
                body = %request.body,
                "API request"
            );

            let bytes = http::send(&request).await?;
            Ok(self.event_stream(bytes, opts.tools))
        })
    }
}
