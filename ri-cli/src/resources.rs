// Resource loading and system prompt construction.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use serde::{Deserialize, Serialize};
use ri_core::types::{Model, ModelCost};

const CONTEXT_FILE_NAMES: &[&str] = &["AGENTS.md", "CLAUDE.md"];

// -- Settings --

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Settings {
    #[serde(default, rename = "defaultModel")]
    pub default_model: Option<String>,
    #[serde(default, rename = "defaultProvider")]
    pub default_provider: Option<String>,
    #[serde(default, rename = "defaultThinking")]
    pub default_thinking: Option<String>,
}

// -- models.json types --

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelsConfig {
    #[serde(default)]
    pub providers: HashMap<String, ProviderConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    #[serde(default, rename = "baseUrl")]
    pub base_url: Option<String>,
    #[serde(default, rename = "apiKey")]
    pub api_key: Option<String>,
    #[serde(default)]
    pub models: Vec<ModelDef>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelDef {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub reasoning: bool,
    #[serde(default)]
    pub cost: Option<ModelCostDef>,
    #[serde(default, rename = "contextWindow")]
    pub context_window: Option<usize>,
    #[serde(default, rename = "maxTokens")]
    pub max_tokens: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelCostDef {
    #[serde(default)] pub input: f64,
    #[serde(default)] pub output: f64,
    #[serde(default, rename = "cacheRead")] pub cache_read: f64,
    #[serde(default, rename = "cacheWrite")] pub cache_write: f64,
}

// -- Discovery types --

pub struct ContextFile { pub path: PathBuf, pub content: String }

// -- ResourceLoader --

pub struct ResourceLoader {
    pub context_files: Vec<ContextFile>,
    pub settings: Settings,
    pub models_config: Option<ModelsConfig>,
}

impl ResourceLoader {
    pub fn load(cwd: &Path) -> Self {
        let global_dir = dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")).join(".ri");
        let cwd = cwd.to_path_buf();

        Self {
            settings: load_settings(&global_dir),
            models_config: load_models_config(&global_dir),
            context_files: discover_context_files(&global_dir, &cwd),
        }
    }

    pub fn custom_models(&self) -> Vec<Model> {
        let config = match &self.models_config {
            Some(c) => c,
            None => return Vec::new(),
        };
        let mut out = Vec::new();
        for (_provider_name, provider_cfg) in &config.providers {
            for m in &provider_cfg.models {
                let cost = m.cost.as_ref().map_or(
                    ModelCost { input: 0.0, output: 0.0, cache_read: 0.0, cache_write: 0.0 },
                    |c| ModelCost { input: c.input, output: c.output, cache_read: c.cache_read, cache_write: c.cache_write },
                );
                out.push(Model {
                    id: m.id.clone(), name: m.name.clone(),
                    reasoning: m.reasoning,
                    context_window: m.context_window.unwrap_or(128_000),
                    max_tokens: m.max_tokens.unwrap_or(16_384),
                    cost,
                });
            }
        }
        out
    }

    pub fn provider_api_key(&self, provider: &str) -> Option<String> {
        self.models_config.as_ref()?.providers.get(provider)?.api_key.clone()
    }

    pub fn build_system_prompt(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        parts.push(BASE_SYSTEM_PROMPT.to_string());

        if !self.context_files.is_empty() {
            parts.push("# Context Files".to_string());
            for cf in &self.context_files {
                parts.push(format!("## {}\n\n{}", cf.path.display(), cf.content));
            }
        }

        parts.join("\n\n")
    }
}

const BASE_SYSTEM_PROMPT: &str = r#"You are ri, a coding agent. You help with software engineering tasks: fixing bugs, adding features, refactoring code, and more.

You have access to tools for reading, writing, and searching code. Use them to understand the codebase before making changes.

Be concise. Focus on what the user asked for. Do not over-engineer."#;

// -- Discovery functions --

fn load_settings(global_dir: &Path) -> Settings {
    let path = global_dir.join("settings.json");
    std::fs::read_to_string(&path).ok()
        .and_then(|c| serde_json::from_str(&c).ok())
        .unwrap_or_default()
}

fn load_models_config(global_dir: &Path) -> Option<ModelsConfig> {
    let content = std::fs::read_to_string(global_dir.join("models.json")).ok()?;
    serde_json::from_str(&content).ok()
}

fn discover_context_files(global_dir: &Path, cwd: &Path) -> Vec<ContextFile> {
    let mut files = Vec::new();
    let mut seen = std::collections::HashSet::new();

    let mut dir = Some(cwd.to_path_buf());
    while let Some(d) = dir {
        for name in CONTEXT_FILE_NAMES {
            let p = d.join(name);
            if p.is_file() {
                if let Ok(content) = std::fs::read_to_string(&p) {
                    let canonical = p.canonicalize().unwrap_or_else(|_| p.clone());
                    if seen.insert(canonical) {
                        files.push(ContextFile { path: p, content });
                    }
                }
            }
        }
        dir = d.parent().map(|p| p.to_path_buf());
    }

    for name in CONTEXT_FILE_NAMES {
        let p = global_dir.join(name);
        if p.is_file() {
            if let Ok(content) = std::fs::read_to_string(&p) {
                let canonical = p.canonicalize().unwrap_or_else(|_| p.clone());
                if seen.insert(canonical) {
                    files.push(ContextFile { path: p, content });
                }
            }
        }
    }

    files
}
