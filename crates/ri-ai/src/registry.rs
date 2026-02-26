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
    "claude-opus-4-6"
}

/// All model IDs across every registered provider.
pub fn available_model_ids() -> Vec<String> {
    FACTORIES.iter()
        .flat_map(|f| f().models())
        .map(|m| m.id)
        .collect()
}

/// Resolve a model ID to its provider, only constructing the matching provider.
/// Supports exact matches and prefix matches (e.g. "claude-opus-4-6" matches
/// "claude-opus-4-6-20250610").
pub async fn resolve(model_id: &str) -> eyre::Result<(Box<dyn LlmProvider>, Model)> {
    // Exact match first.
    for factory in FACTORIES {
        let provider = factory();
        if let Some(model) = provider.models().into_iter().find(|m| m.id == model_id) {
            return Ok((provider, model));
        }
    }

    // Prefix match fallback.
    for factory in FACTORIES {
        let provider = factory();
        if let Some(model) = provider.models().into_iter().find(|m| m.id.starts_with(model_id)) {
            return Ok((provider, model));
        }
    }

    let available = available_model_ids();
    Err(eyre::eyre!("Unknown model '{}'. Available: {}", model_id, available.join(", ")))
}
