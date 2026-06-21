// Context file discovery and settings for ri.
//
// Shared between ri-cli and ri-web. Discovers AGENTS.md / CLAUDE.md / README.md
// from the global config directory (~/.config/agents/) and project-local
// locations, walking up from the working directory.

use std::collections::HashSet;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The file names discovered as agent context, in injection-priority order.
/// AGENTS.md and CLAUDE.md carry agent instructions; README.md rides along to
/// give the agent a project's human-facing overview wherever one lives. Matched
/// case-insensitively (see `scan_dir`), so a third-party project's `readme.md`
/// or `Readme.md` is found just as readily as `README.md`.
pub const CONTEXT_FILE_NAMES: &[&str] = &["AGENTS.md", "CLAUDE.md", "README.md"];

/// Maximum directory levels to walk upward before stopping. Prevents
/// unbounded traversal when the starting directory is deeply nested.
pub const MAX_WALK_DEPTH: usize = 25;

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

/// Walk up from `dir`, yielding each ancestor directory (closest first)
/// up to `MAX_WALK_DEPTH` levels. Warns if the depth limit is reached.
pub fn walk_ancestors(dir: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let mut current = Some(dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf()));
    while let Some(d) = current {
        if dirs.len() >= MAX_WALK_DEPTH {
            tracing::warn!(
                "upward directory walk from [{}] hit the depth limit of {} at [{}]",
                dir.display(),
                MAX_WALK_DEPTH,
                d.display(),
            );
            break;
        }
        dirs.push(d.clone());
        current = d.parent().map(|p| p.to_path_buf());
    }
    dirs
}

/// Walk up from `dir`, collecting context files (AGENTS.md, CLAUDE.md,
/// README.md). At each level checks `.agents/` subdirectory then the directory
/// itself. Returns files in walk order (closest first).
pub fn find_context_files(dir: &Path) -> Vec<ContextFile> {
    let mut files = Vec::new();
    let mut seen = HashSet::new();
    collect_walk(dir, &mut files, &mut seen);
    files
}

/// Discover context files for the agent system prompt.
///
/// Returns files in prompt order:
/// 1. Global: ~/.config/agents/ (AGENTS.md, CLAUDE.md)
/// 2. Project-local: walk up from cwd via `find_context_files`.
pub fn discover_context_files(cwd: &Path) -> Vec<ContextFile> {
    let mut files = Vec::new();
    let mut seen = HashSet::new();

    // Global config.
    if let Some(global) = config_dir() {
        scan_dir(&global, &mut files, &mut seen);
    }

    // Project-local: walk up from cwd.
    collect_walk(cwd, &mut files, &mut seen);

    files
}

/// Format context files into a prompt section.
///
/// Each file gets a markdown header with its path and its content below.
/// Returns an empty string if there are no context files.
pub fn format_context_files(context_files: &[ContextFile]) -> String {
    if context_files.is_empty() {
        return String::new();
    }
    let mut parts = vec!["# Context Files".to_string()];
    for cf in context_files {
        parts.push(format!("## {}\n\n{}", cf.path.display(), cf.content));
    }
    parts.join("\n\n")
}

/// Build an environment info block for the system prompt.
///
/// Gathers platform, OS, git status, date, and working directory so the LLM
/// has situational awareness. Mirrors our pi env-info extension.
pub fn get_environment_system_prompt(additional_lines: Option<Vec<String>>) -> String {
    let platform = env::consts::OS;
    let arch = env::consts::ARCH;
    let os = os_info::get();
    let os_version = format!("{} {}", os.os_type(), os.version());
    let date = chrono::Local::now().format("%Y-%m-%d").to_string();

    let mut lines = vec![
        format!("Platform: {platform}"),
        format!("Architecture: {arch}"),
        format!("OS version: {os_version}"),
        format!("Date of first message: {date}"),
    ];

    if let Some(additional_lines) = additional_lines {
        for line in additional_lines {
            lines.push(line);
        }
    }

    format!(
        "\nHere is useful information about the environment you are running in:\n\n<env>\n\n{}\n\n</env>",
        lines.join("\\\n"),
    )
}

pub const BASE_SYSTEM_PROMPT: &str = include_str!("prompts/base_system.md");

// -- Internal --

/// Walk up from `dir`, scanning at each level. Shared by find_context_files
/// and discover_context_files (which seeds `seen` with global files first).
fn collect_walk(dir: &Path, files: &mut Vec<ContextFile>, seen: &mut HashSet<PathBuf>) {
    for d in walk_ancestors(dir) {
        scan_dir(&d.join(".agents"), files, seen);
        scan_dir(&d, files, seen);
    }
}

/// Scan a directory for context files (AGENTS.md, CLAUDE.md, README.md),
/// matching each name case-insensitively so a project's `readme.md` reads the
/// same as `README.md`. Names are tried in `CONTEXT_FILE_NAMES` priority order.
/// Each discovered file is the root of its own include graph; its parent
/// directory becomes the boundary that includes cannot escape above.
fn scan_dir(dir: &Path, files: &mut Vec<ContextFile>, seen: &mut HashSet<PathBuf>) {
    let names = dir_filenames(dir);
    for target in CONTEXT_FILE_NAMES {
        for name in names.iter().filter(|n| n.eq_ignore_ascii_case(target)) {
            let p = dir.join(name);
            if !p.is_file() {
                continue;
            }
            let Ok(raw) = fs::read_to_string(&p) else { continue };
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

/// The sorted file names directly inside `dir`. A missing directory yields an
/// empty list silently -- a `.agents/` subdir usually doesn't exist, and that is
/// not an anomaly -- while any other read error is surfaced as a warning and
/// likewise treated as empty. Sorting makes discovery order deterministic
/// regardless of how the filesystem happens to enumerate entries.
fn dir_filenames(dir: &Path) -> Vec<String> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(e) => {
            tracing::warn!(
                "could not read directory [{}] while discovering context files: {}",
                dir.display(),
                e,
            );
            return Vec::new();
        }
    };
    let mut names: Vec<String> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// Expand `{{include:path}}` directives in `content`, recursively.
///
/// - `base_dir`: directory of the file containing `content`, for resolving
///   relative include paths.
/// - `boundary`: the root file's parent directory. Included files must
///   resolve within this directory (no upward traversal past it).
/// - `visited`: canonical paths already in the include stack, for cycle
///   detection.
///
/// Shared by AGENTS.md/CLAUDE.md/README.md discovery and the glob-rule discovery
/// in ri-web (for global rules, which live on the server's own filesystem), so
/// every context prompt file expands includes the same way.
pub fn expand_includes(
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
