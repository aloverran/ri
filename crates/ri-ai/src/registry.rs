// Provider registry -- simple list of all providers.

use ri::{LlmProvider, Model, ModelCost, ProviderInfo};
use crate::{AnthropicProvider, GeminiProvider, GeminiVariant};

pub fn all_providers() -> Vec<Box<dyn LlmProvider>> {
    vec![
        Box::new(AnthropicProvider::new()),
        Box::new(GeminiProvider::new(GeminiVariant::Cli)),
        Box::new(GeminiProvider::new(GeminiVariant::Antigravity)),
    ]
}

pub fn default_model_id() -> &'static str {
    "claude-sonnet-4-20250514"
}

// Resolve a provider + model for the given model id.
pub async fn resolve(model_id: &str) -> eyre::Result<(Box<dyn LlmProvider>, Model)> {
    for provider in all_providers() {
        for model in provider.models() {
            if model.id == model_id {
                return Ok((provider, model));
            }
        }
    }

    // Fallback: unknown model, use first provider with a default model.
    let provider = Box::new(AnthropicProvider::new());
    let model = Model {
        id: model_id.into(), name: model_id.into(),
        reasoning: false, context_window: 128_000, max_tokens: 16_384,
        cost: ModelCost { input: 0.0, output: 0.0, cache_read: 0.0, cache_write: 0.0 },
    };
    Ok((provider, model))
}

// Info about all available providers, for display in login UI etc.
pub fn provider_info() -> Vec<ProviderInfo> {
    all_providers().iter().map(|p| ProviderInfo {
        id: Box::leak(p.id().to_string().into_boxed_str()),
        name: Box::leak(p.name().to_string().into_boxed_str()),
    }).collect()
}
