// Model registry -- discovers and manages available models + API keys.

use std::collections::HashMap;
use std::sync::Arc;

use ri_core::provider::LlmProvider;
use ri_core::types::Model;

pub struct ModelRegistry {
    providers: HashMap<String, Arc<dyn LlmProvider>>,
    models: Vec<Model>,
}

impl ModelRegistry {
    pub fn new() -> Self {
        Self {
            providers: HashMap::new(),
            models: Vec::new(),
        }
    }

    pub fn register_provider(&mut self, name: &str, provider: Arc<dyn LlmProvider>) {
        self.providers.insert(name.to_string(), provider);
    }

    pub fn add_model(&mut self, model: Model) {
        self.models.push(model);
    }

    pub fn find(&self, provider: &str, model_id: &str) -> Option<&Model> {
        self.models
            .iter()
            .find(|m| m.provider == provider && m.id == model_id)
    }

    pub fn get_provider(&self, name: &str) -> Option<Arc<dyn LlmProvider>> {
        self.providers.get(name).cloned()
    }

    pub fn available_models(&self) -> &[Model] {
        &self.models
    }

    /// Resolve API key for a provider.
    /// Checks: env var -> shell command (!) -> literal.
    pub async fn resolve_api_key(&self, key_spec: &str) -> Option<String> {
        // Check if it's an env var name
        if let Ok(val) = std::env::var(key_spec) {
            return Some(val);
        }

        // Check if it's a shell command (starts with !)
        if let Some(cmd) = key_spec.strip_prefix('!') {
            let output = tokio::process::Command::new("sh")
                .arg("-c")
                .arg(cmd)
                .output()
                .await
                .ok()?;
            if output.status.success() {
                return Some(String::from_utf8_lossy(&output.stdout).trim().to_string());
            }
            return None;
        }

        // Use as literal
        Some(key_spec.to_string())
    }
}
