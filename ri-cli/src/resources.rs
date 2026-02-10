// Resource loading and system prompt construction.

use std::path::{Path, PathBuf};
use serde::{Deserialize, Serialize};

const CONTEXT_FILE_NAMES: &[&str] = &["AGENTS.md", "CLAUDE.md"];

// -- Settings --

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Settings {
    #[serde(default, rename = "defaultModel")]
    pub default_model: Option<String>,
    #[serde(default, rename = "defaultThinking")]
    pub default_thinking: Option<String>,
}

// -- Discovery types --

pub struct ContextFile { pub path: PathBuf, pub content: String }

// -- ResourceLoader --

pub struct ResourceLoader {
    pub context_files: Vec<ContextFile>,
    pub settings: Settings,
}

impl ResourceLoader {
    pub fn load(cwd: &Path) -> Self {
        let global_dir = dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")).join(".ri");
        let cwd = cwd.to_path_buf();

        Self {
            settings: load_settings(&global_dir),
            context_files: discover_context_files(&global_dir, &cwd),
        }
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
