// Provider registry -- simple list of all providers.

use ri::{LlmProvider, Model};
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

pub async fn resolve(model_id: &str) -> eyre::Result<(Box<dyn LlmProvider>, Model)> {
    for provider in all_providers() {
        for model in provider.models() {
            if model.id == model_id {
                return Ok((provider, model));
            }
        }
    }

    let available: Vec<String> = all_providers().iter()
        .flat_map(|p| p.models())
        .map(|m| m.id)
        .collect();
    Err(eyre::eyre!("Unknown model '{}'. Available: {}", model_id, available.join(", ")))
}
