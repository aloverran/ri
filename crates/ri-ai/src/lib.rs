pub mod sse;
pub mod http;
pub mod anthropic;
pub mod gemini;
pub mod pkce;
pub mod registry;

pub use anthropic::AnthropicProvider;
pub use gemini::{GeminiProvider, GeminiVariant};

pub fn epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
