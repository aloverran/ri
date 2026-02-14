// Provider registry -- lazy construction to avoid unnecessary credential loading.

use ri::{LlmProvider, Model};
use crate::{AnthropicProvider, GeminiProvider, GeminiVariant};

type ProviderFactory = fn() -> Box<dyn LlmProvider>;

const FACTORIES: &[ProviderFactory] = &[
    || Box::new(AnthropicProvider::new()),
    || Box::new(GeminiProvider::new(GeminiVariant::Cli)),
    || Box::new(GeminiProvider::new(GeminiVariant::Antigravity)),
];

pub fn all_providers() -> Vec<Box<dyn LlmProvider>> {
    FACTORIES.iter().map(|f| f()).collect()
}

pub fn default_model_id() -> &'static str {
    "claude-sonnet-4-20250514"
}

/// Resolve a model ID to its provider, only constructing the matching provider.
pub async fn resolve(model_id: &str) -> eyre::Result<(Box<dyn LlmProvider>, Model)> {
    for factory in FACTORIES {
        let provider = factory();
        if let Some(model) = provider.models().into_iter().find(|m| m.id == model_id) {
            return Ok((provider, model));
        }
    }

    let available: Vec<String> = FACTORIES.iter()
        .flat_map(|f| f().models())
        .map(|m| m.id)
        .collect();
    Err(eyre::eyre!("Unknown model '{}'. Available: {}", model_id, available.join(", ")))
}
