// Provider registry -- lazy construction to avoid unnecessary credential loading.

use ri::{LlmProvider, Model};
use crate::{AnthropicProvider, GeminiProvider, GeminiVariant, OpenAICodexProvider};

type ProviderFactory = fn() -> Box<dyn LlmProvider>;

const FACTORIES: &[ProviderFactory] = &[
    || Box::new(AnthropicProvider::new()),
    || Box::new(GeminiProvider::new(GeminiVariant::Cli)),
    || Box::new(GeminiProvider::new(GeminiVariant::Antigravity)),
    || Box::new(GeminiProvider::new(GeminiVariant::ApiKey)),
    || Box::new(OpenAICodexProvider::new()),
];

pub fn all_providers() -> Vec<Box<dyn LlmProvider>> {
    FACTORIES.iter().map(|f| f()).collect()
}

pub fn default_model_id() -> &'static str {
    "claude-opus-4-8"
}

/// Model IDs from authenticated providers only.
/// Used in tool descriptions and error messages -- the caller should only
/// see models they can actually use.
pub fn available_model_ids() -> Vec<String> {
    FACTORIES.iter()
        .map(|f| f())
        .filter(|p| p.is_authenticated())
        .flat_map(|p| p.models())
        .map(|m| m.id)
        .collect()
}

/// Resolve a model ID to its provider, only constructing the matching provider.
/// Supports exact matches and prefix matches (e.g. "claude-opus-4-6" matches
/// "claude-opus-4-6-20250610").
///
/// Returns an error if the model is found but its provider is not authenticated.
/// This catches unauthenticated usage early -- before any background task or
/// API call is attempted -- so callers get a clear, synchronous error.
pub async fn resolve(model_id: &str) -> eyre::Result<(Box<dyn LlmProvider>, Model)> {
    // Exact match first.
    for factory in FACTORIES {
        let provider = factory();
        if let Some(model) = provider.models().into_iter().find(|m| m.id == model_id) {
            return require_auth(provider, model);
        }
    }

    // Prefix match fallback.
    for factory in FACTORIES {
        let provider = factory();
        if let Some(model) = provider.models().into_iter().find(|m| m.id.starts_with(model_id)) {
            return require_auth(provider, model);
        }
    }

    let available = available_model_ids();
    Err(eyre::eyre!("Unknown model '{}'. Available: {}", model_id, available.join(", ")))
}

fn require_auth(provider: Box<dyn LlmProvider>, model: Model) -> eyre::Result<(Box<dyn LlmProvider>, Model)> {
    if provider.is_authenticated() {
        Ok((provider, model))
    } else {
        Err(eyre::eyre!(
            "Model '{}' is not available -- provider '{}' is not logged in",
            model.id, provider.name()
        ))
    }
}
