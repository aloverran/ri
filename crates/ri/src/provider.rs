//! LLM provider contract.
//!
//! Defines the abstractions for LLM backends: model metadata, the provider
//! trait, request/error types, and auth flow descriptions. Higher layers
//! (ri-ai) implement `LlmProvider` for each backend (Anthropic, Gemini, etc).

use std::pin::Pin;
use std::{fmt, str::FromStr};

use async_trait::async_trait;
use futures::Stream;
use serde::{Deserialize, Serialize};

use crate::model::Message;
use crate::stream::StreamEvent;
use crate::tool::ToolSchema;

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

impl fmt::Display for ThinkingLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Off => "off",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
        })
    }
}

impl FromStr for ThinkingLevel {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "off" => Ok(Self::Off),
            "low" => Ok(Self::Low),
            "medium" => Ok(Self::Medium),
            "high" => Ok(Self::High),
            "xhigh" => Ok(Self::XHigh),
            other => Err(format!("unknown thinking level '{}'", other)),
        }
    }
}

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

type BoxError = Box<dyn std::error::Error + Send + Sync + 'static>;

/// An error from an LLM provider. Actionable variants (`RateLimited`,
/// `ContextOverflow`) carry structured data for the agent loop to act on.
/// Everything else flows through `Other` transparently, preserving the
/// full source chain for diagnostics.
#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("rate limited, retry after {retry_after_ms}ms")]
    RateLimited {
        retry_after_ms: u64,
        source: BoxError,
    },

    #[error("context overflow")]
    ContextOverflow {
        source: BoxError,
    },

    #[error(transparent)]
    Other(BoxError),
}

impl ApiError {
    pub fn other(source: impl Into<BoxError>) -> Self {
        Self::Other(source.into())
    }

    pub fn rate_limited(retry_after_ms: u64, source: impl Into<BoxError>) -> Self {
        Self::RateLimited { retry_after_ms, source: source.into() }
    }

    pub fn context_overflow(source: impl Into<BoxError>) -> Self {
        Self::ContextOverflow { source: source.into() }
    }

    /// Walk the full source chain into a single string for logging or persistence.
    pub fn display_chain(&self) -> String {
        use std::error::Error;
        let mut chain = self.to_string();
        let mut current = self.source();
        while let Some(cause) = current {
            chain.push_str(": ");
            chain.push_str(&cause.to_string());
            current = cause.source();
        }
        chain
    }
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
