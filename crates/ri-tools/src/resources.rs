// Context file discovery and settings for ri.
//
// Shared between ri-cli and ri-web. Discovers AGENTS.md / CLAUDE.md from
// the global config directory (~/.config/agents/) and project-local locations,
// walking up from the working directory.

use std::collections::HashSet;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const CONTEXT_FILE_NAMES: &[&str] = &["AGENTS.md", "CLAUDE.md"];

/// A context file discovered on disk.
pub struct ContextFile {
    pub path: PathBuf,
    pub content: String,
}

/// User settings from ~/.config/agents/settings.json.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Settings {
    #[serde(default, rename = "defaultModel")]
    pub default_model: Option<String>,
    #[serde(default, rename = "defaultThinking")]
    pub default_thinking: Option<String>,
}

/// The global config directory: ~/.config/agents/
pub fn config_dir() -> Option<PathBuf> {
    env::var("HOME").ok().map(|h| PathBuf::from(h).join(".config").join("agents"))
}

/// Load settings from ~/.config/agents/settings.json.
pub fn load_settings() -> Settings {
    config_dir()
        .map(|d| d.join("settings.json"))
        .and_then(|p| fs::read_to_string(p).ok())
        .and_then(|c| serde_json::from_str(&c).ok())
        .unwrap_or_default()
}

/// Discover context files for the agent system prompt.
///
/// Returns files in prompt order:
/// 1. Global: ~/.config/agents/ (AGENTS.md, CLAUDE.md)
/// 2. Project-local: walk up from cwd, at each level checking
///    .agents/ directory then bare files. Stops at .git boundary.
pub fn discover_context_files(cwd: &Path) -> Vec<ContextFile> {
    let mut files = Vec::new();
    let mut seen = HashSet::new();

    // Global config.
    if let Some(global) = config_dir() {
        scan_dir(&global, &mut files, &mut seen);
    }

    // Project-local: walk up from cwd.
    // Canonicalize so parent() walks correctly even if cwd was relative.
    let mut dir = Some(cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf()));
    while let Some(d) = dir {
        scan_dir(&d.join(".agents"), &mut files, &mut seen);
        scan_dir(&d, &mut files, &mut seen);
        if d.join(".git").exists() { break; }
        dir = d.parent().map(|p| p.to_path_buf());
    }

    files
}

/// Build the default system prompt from discovered context files.
pub fn build_system_prompt(context_files: &[ContextFile]) -> String {
    let mut parts: Vec<String> = vec![BASE_SYSTEM_PROMPT.to_string()];

    if !context_files.is_empty() {
        parts.push("# Context Files".to_string());
        for cf in context_files {
            parts.push(format!("## {}\n\n{}", cf.path.display(), cf.content));
        }
    }

    parts.join("\n\n")
}

pub const BASE_SYSTEM_PROMPT: &str = "\
You are ri, a coding agent. You help with software engineering tasks: \
fixing bugs, adding features, refactoring code, and more.\n\n\
You have access to tools for reading, writing, and searching code. \
Use them to understand the codebase before making changes.\n\n\
Be concise. Focus on what the user asked for. Do not over-engineer.";

// -- Internal --

/// Scan a directory for context files (AGENTS.md, CLAUDE.md).
fn scan_dir(dir: &Path, files: &mut Vec<ContextFile>, seen: &mut HashSet<PathBuf>) {
    for name in CONTEXT_FILE_NAMES {
        let p = dir.join(name);
        if p.is_file() {
            if let Ok(content) = fs::read_to_string(&p) {
                let canonical = p.canonicalize().unwrap_or_else(|_| p.clone());
                if seen.insert(canonical) {
                    files.push(ContextFile { path: p, content });
                }
            }
        }
    }
}
