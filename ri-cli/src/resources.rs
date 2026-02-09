// Resource loading and system prompt construction.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use serde::{Deserialize, Serialize};
use ri::types::{Model, ModelCost};

const CONTEXT_FILE_NAMES: &[&str] = &["AGENTS.md", "CLAUDE.md"];

// -- Settings --

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactionSettings {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_reserve_tokens", rename = "reserveTokens")]
    pub reserve_tokens: usize,
    #[serde(default = "default_keep_recent_tokens", rename = "keepRecentTokens")]
    pub keep_recent_tokens: usize,
}

fn default_true() -> bool { true }
fn default_reserve_tokens() -> usize { 16384 }
fn default_keep_recent_tokens() -> usize { 20000 }

impl Default for CompactionSettings {
    fn default() -> Self {
        Self { enabled: true, reserve_tokens: 16384, keep_recent_tokens: 20000 }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Settings {
    #[serde(default)]
    pub compaction: CompactionSettings,
    #[serde(default, rename = "defaultModel")]
    pub default_model: Option<String>,
    #[serde(default, rename = "defaultProvider")]
    pub default_provider: Option<String>,
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
pub struct Skill { pub name: String, pub description: String, pub content: String, pub path: PathBuf }
pub struct PromptTemplate { pub name: String, pub content: String, pub path: PathBuf }

// -- ResourceLoader --

pub struct ResourceLoader {
    pub global_dir: PathBuf,
    pub cwd: PathBuf,
    pub context_files: Vec<ContextFile>,
    pub skills: Vec<Skill>,
    pub prompts: Vec<PromptTemplate>,
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
            skills: discover_skills(&global_dir, &cwd),
            prompts: discover_prompts(&global_dir, &cwd),
            global_dir,
            cwd,
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

    pub fn build_system_prompt(&self, active_skill: Option<&str>) -> String {
        let mut parts: Vec<String> = Vec::new();
        parts.push(BASE_SYSTEM_PROMPT.to_string());

        if !self.context_files.is_empty() {
            parts.push("# Context Files".to_string());
            for cf in &self.context_files {
                parts.push(format!("## {}\n\n{}", cf.path.display(), cf.content));
            }
        }

        if let Some(skill_name) = active_skill {
            if let Some(skill) = self.skills.iter().find(|s| s.name == skill_name) {
                parts.push(format!("# Active Skill: {}\n\n{}", skill.name, skill.content));
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

fn discover_skills(global_dir: &Path, cwd: &Path) -> Vec<Skill> {
    let mut skills = Vec::new();
    let mut seen = std::collections::HashSet::new();

    let mut dir = Some(cwd.to_path_buf());
    while let Some(d) = dir {
        collect_skills_from(&d.join(".ri").join("skills"), &mut skills, &mut seen);
        dir = d.parent().map(|p| p.to_path_buf());
    }
    collect_skills_from(&global_dir.join("skills"), &mut skills, &mut seen);

    skills
}

fn collect_skills_from(skills_dir: &Path, out: &mut Vec<Skill>, seen: &mut std::collections::HashSet<String>) {
    let entries = match std::fs::read_dir(skills_dir) { Ok(e) => e, Err(_) => return };
    for entry in entries.flatten() {
        if !entry.file_type().map_or(false, |ft| ft.is_dir()) { continue; }
        let skill_md = entry.path().join("SKILL.md");
        if !skill_md.is_file() { continue; }
        let content = match std::fs::read_to_string(&skill_md) { Ok(c) => c, Err(_) => continue };
        let name = entry.file_name().to_string_lossy().to_string();
        if seen.contains(&name) { continue; }
        let description = parse_frontmatter_field(&content, "description").unwrap_or_default();
        seen.insert(name.clone());
        out.push(Skill { name, description, content: strip_frontmatter(&content), path: skill_md });
    }
}

fn discover_prompts(global_dir: &Path, cwd: &Path) -> Vec<PromptTemplate> {
    let mut prompts = Vec::new();
    let mut seen = std::collections::HashSet::new();

    let mut dir = Some(cwd.to_path_buf());
    while let Some(d) = dir {
        collect_prompts_from(&d.join(".ri").join("prompts"), &mut prompts, &mut seen);
        dir = d.parent().map(|p| p.to_path_buf());
    }
    collect_prompts_from(&global_dir.join("prompts"), &mut prompts, &mut seen);

    prompts
}

fn collect_prompts_from(prompts_dir: &Path, out: &mut Vec<PromptTemplate>, seen: &mut std::collections::HashSet<String>) {
    let entries = match std::fs::read_dir(prompts_dir) { Ok(e) => e, Err(_) => return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") { continue; }
        let content = match std::fs::read_to_string(&path) { Ok(c) => c, Err(_) => continue };
        let stem = path.file_stem().unwrap_or_default().to_string_lossy().to_string();
        if seen.contains(&stem) { continue; }
        let name = parse_frontmatter_field(&content, "name").unwrap_or_else(|| stem.clone());
        seen.insert(stem);
        out.push(PromptTemplate { name, content: strip_frontmatter(&content), path });
    }
}

fn parse_frontmatter_field(content: &str, key: &str) -> Option<String> {
    let fm = extract_frontmatter(content)?;
    let prefix = format!("{}:", key);
    for line in fm.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with(&prefix) {
            let value = trimmed[prefix.len()..].trim();
            let value = value.strip_prefix('"').and_then(|v| v.strip_suffix('"'))
                .or_else(|| value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
                .unwrap_or(value);
            return Some(value.to_string());
        }
    }
    None
}

fn extract_frontmatter(content: &str) -> Option<&str> {
    let content = content.trim_start();
    if !content.starts_with("---") { return None; }
    let after = &content[3..];
    let end = after.find("\n---")?;
    Some(&after[..end])
}

fn strip_frontmatter(content: &str) -> String {
    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") { return content.to_string(); }
    let after = &trimmed[3..];
    match after.find("\n---") {
        Some(end) => after[end + 4..].trim_start_matches('\n').to_string(),
        None => content.to_string(),
    }
}
