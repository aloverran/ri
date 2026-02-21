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
/// Each discovered file is the root of its own include graph; its parent
/// directory becomes the boundary that includes cannot escape above.
fn scan_dir(dir: &Path, files: &mut Vec<ContextFile>, seen: &mut HashSet<PathBuf>) {
    for name in CONTEXT_FILE_NAMES {
        let p = dir.join(name);
        if p.is_file() {
            if let Ok(raw) = fs::read_to_string(&p) {
                let canonical = p.canonicalize().unwrap_or_else(|_| p.clone());
                if seen.insert(canonical.clone()) {
                    let boundary = canonical.parent()
                        .map(|d| d.to_path_buf())
                        .unwrap_or_else(|| canonical.clone());
                    let mut visited = HashSet::new();
                    visited.insert(canonical);
                    let content = expand_includes(&raw, dir, &boundary, &mut visited);
                    files.push(ContextFile { path: p, content });
                }
            }
        }
    }
}

/// Expand `{{include:path}}` directives in `content`, recursively.
///
/// - `base_dir`: directory of the file containing `content`, for resolving
///   relative include paths.
/// - `boundary`: the root file's parent directory. Included files must
///   resolve within this directory (no upward traversal past it).
/// - `visited`: canonical paths already in the include stack, for cycle
///   detection.
fn expand_includes(
    content: &str,
    base_dir: &Path,
    boundary: &Path,
    visited: &mut HashSet<PathBuf>,
) -> String {
    let mut result = String::with_capacity(content.len());
    let mut rest = content;

    while let Some(open) = rest.find("{{include:") {
        result.push_str(&rest[..open]);
        let after_open = &rest[open + "{{include:".len()..];
        match after_open.find("}}") {
            None => {
                // Unterminated directive -- pass through literally.
                result.push_str(&rest[open..]);
                rest = "";
                break;
            }
            Some(close) => {
                let raw_target = after_open[..close].trim();
                let replacement = resolve_include(raw_target, base_dir, boundary, visited);
                result.push_str(&replacement);
                rest = &after_open[close + "}}".len()..];
            }
        }
    }
    result.push_str(rest);
    result
}

/// Resolve a single include target to its (recursively expanded) content,
/// or an error marker if something goes wrong.
fn resolve_include(
    target: &str,
    base_dir: &Path,
    boundary: &Path,
    visited: &mut HashSet<PathBuf>,
) -> String {
    if target.is_empty() {
        let msg = "empty include target";
        eprintln!("[ri] include warning: {}", msg);
        return format!("<!-- include error: {} -->", msg);
    }

    let resolved = base_dir.join(target);
    let canonical = match fs::canonicalize(&resolved) {
        Ok(c) => c,
        Err(_) => {
            let msg = format!("file not found: {}", target);
            eprintln!("[ri] include warning: {}", msg);
            return format!("<!-- include error: {} -->", msg);
        }
    };

    // Boundary check: canonical path must be within (or equal to) the
    // boundary directory. PathBuf::starts_with compares components, so
    // "/foo/bar".starts_with("/foo/b") is false -- this is safe.
    if !canonical.starts_with(boundary) {
        let msg = format!(
            "path escapes boundary: {} (boundary: {})",
            target,
            boundary.display()
        );
        eprintln!("[ri] include warning: {}", msg);
        return format!("<!-- include error: {} -->", msg);
    }

    // Cycle check.
    if !visited.insert(canonical.clone()) {
        let msg = format!("include cycle: {}", target);
        eprintln!("[ri] include warning: {}", msg);
        return format!("<!-- include error: {} -->", msg);
    }

    let raw = match fs::read_to_string(&canonical) {
        Ok(c) => c,
        Err(e) => {
            let msg = format!("cannot read {}: {}", target, e);
            eprintln!("[ri] include warning: {}", msg);
            visited.remove(&canonical);
            return format!("<!-- include error: {} -->", msg);
        }
    };

    let include_dir = canonical.parent()
        .map(|d| d.to_path_buf())
        .unwrap_or_else(|| base_dir.to_path_buf());
    let expanded = expand_includes(&raw, &include_dir, boundary, visited);

    visited.remove(&canonical);
    expanded
}
