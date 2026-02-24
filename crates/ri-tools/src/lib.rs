// Built-in tools for ri.

pub mod prompts;
pub mod resources;

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use tracing::Instrument;
use ri::{Tool, ToolOutput};

pub fn all_tools() -> Vec<Box<dyn Tool>> {
    vec![Box::new(BashTool), Box::new(ReadTool), Box::new(WriteTool), Box::new(EditTool)]
}

struct BashTool;

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &str { "bash" }
    fn description(&self) -> &str { "Execute a shell command" }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "The shell command to execute" },
                "timeout": { "type": "integer", "description": "Timeout in milliseconds" }
            },
            "required": ["command"]
        })
    }
    async fn run(&self, input: serde_json::Value, cwd: PathBuf, cancel: tokio_util::sync::CancellationToken) -> ToolOutput {
        run_bash(input, cwd, cancel)
            .instrument(tracing::info_span!("tool", name = "bash"))
            .await
    }
}

struct ReadTool;

#[async_trait]
impl Tool for ReadTool {
    fn name(&self) -> &str { "read" }
    fn description(&self) -> &str { "Read a file's contents. You **MUST** use this instead of `cat` for reading files." }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File path to read" },
                "offset": { "type": "integer", "description": "Starting line (1-indexed)" },
                "limit": { "type": "integer", "description": "Number of lines to read" }
            },
            "required": ["path"]
        })
    }
    async fn run(&self, input: serde_json::Value, cwd: PathBuf, _cancel: tokio_util::sync::CancellationToken) -> ToolOutput {
        run_read(input, cwd)
            .instrument(tracing::info_span!("tool", name = "read"))
            .await
    }
}

struct WriteTool;

#[async_trait]
impl Tool for WriteTool {
    fn name(&self) -> &str { "write" }
    fn description(&self) -> &str { "Write content to a file" }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File path to write" },
                "content": { "type": "string", "description": "Content to write" }
            },
            "required": ["path", "content"]
        })
    }
    async fn run(&self, input: serde_json::Value, cwd: PathBuf, _cancel: tokio_util::sync::CancellationToken) -> ToolOutput {
        run_write(input, cwd)
            .instrument(tracing::info_span!("tool", name = "write"))
            .await
    }
}

struct EditTool;

#[async_trait]
impl Tool for EditTool {
    fn name(&self) -> &str { "edit" }
    fn description(&self) -> &str { "Replace text in a file (exact match)" }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "old_text": { "type": "string", "description": "Exact text to find and replace" },
                "new_text": { "type": "string", "description": "Replacement text" }
            },
            "required": ["path", "old_text", "new_text"]
        })
    }
    async fn run(&self, input: serde_json::Value, cwd: PathBuf, _cancel: tokio_util::sync::CancellationToken) -> ToolOutput {
        run_edit(input, cwd)
            .instrument(tracing::info_span!("tool", name = "edit"))
            .await
    }
}

// -- Tool implementations --

async fn run_bash(
    input: serde_json::Value,
    cwd: PathBuf,
    cancel: tokio_util::sync::CancellationToken,
) -> ToolOutput {
    use command_group::AsyncCommandGroup;

    let command = match input["command"].as_str() {
        Some(c) => c,
        None => return ToolOutput {
            text: "missing 'command' parameter".into(),
            is_error: true,
            details: None,
        },
    };
    let timeout_ms = parse_u64(&input["timeout"]).unwrap_or(120_000);

    let mut cmd = tokio::process::Command::new("sh");
    cmd.arg("-c")
        .arg(command)
        .current_dir(&cwd)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    // group_spawn creates a process group (Unix) or job object (Windows)
    // so we can kill the entire tree on cleanup.
    let mut child = match cmd.group_spawn() {
        Ok(c) => c,
        Err(e) => return ToolOutput {
            text: format!("Failed to spawn: {}", e),
            is_error: true,
            details: None,
        },
    };

    // Take stdout/stderr handles for concurrent reading.
    let inner = child.inner();
    let stdout = inner.stdout.take();
    let stderr = inner.stderr.take();

    let stdout_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        if let Some(mut out) = stdout {
            use tokio::io::AsyncReadExt;
            let _ = out.read_to_end(&mut buf).await;
        }
        buf
    });
    let stderr_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        if let Some(mut err) = stderr {
            use tokio::io::AsyncReadExt;
            let _ = err.read_to_end(&mut buf).await;
        }
        buf
    });

    // Wait for the shell process with timeout and cancellation.
    let exit_status = tokio::select! {
        result = child.inner().wait() => {
            match result {
                Ok(s) => Some(s),
                Err(e) => {
                    let _ = child.kill().await;
                    let _ = stdout_task.await;
                    let _ = stderr_task.await;
                    return ToolOutput {
                        text: format!("Process error: {}", e),
                        is_error: true,
                        details: None,
                    };
                }
            }
        },
        _ = tokio::time::sleep(std::time::Duration::from_millis(timeout_ms)) => None,
        _ = cancel.cancelled() => None,
    };

    // Always kill the process group after the shell exits or on timeout.
    // This cleans up any backgrounded children, which also closes their
    // inherited pipe FDs so the stdout/stderr readers can finish.
    let _ = child.start_kill();

    let stdout_bytes = stdout_task.await.unwrap_or_default();
    let stderr_bytes = stderr_task.await.unwrap_or_default();

    if exit_status.is_none() {
        return ToolOutput {
            text: if cancel.is_cancelled() {
                "Command aborted".into()
            } else {
                format!("Command timed out after {}ms", timeout_ms)
            },
            is_error: true,
            details: None,
        };
    }

    let exit_code = exit_status.unwrap().code().unwrap_or(-1);
    tracing::debug!(exit_code, "bash complete");

    let stdout = String::from_utf8_lossy(&stdout_bytes);
    let stderr = String::from_utf8_lossy(&stderr_bytes);

    let combined = if stderr.is_empty() {
        stdout.to_string()
    } else {
        format!("{}\n--- stderr ---\n{}", stdout, stderr)
    };

    let truncated = truncate_output(&combined, 2000, 50 * 1024);
    let text = format!("Exit code: {}\n{}", exit_code, truncated);

    ToolOutput {
        text,
        is_error: exit_code != 0,
        details: Some(serde_json::json!({
            "exit_code": exit_code,
            "stdout": stdout,
            "stderr": stderr,
        })),
    }
}

async fn run_read(input: serde_json::Value, cwd: PathBuf) -> ToolOutput {
    let path_str = match input["path"].as_str() {
        Some(p) => p,
        None => return ToolOutput {
            text: "missing 'path' parameter".into(),
            is_error: true,
            details: None,
        },
    };

    let path = resolve_path(path_str, &cwd);
    let content = match tokio::fs::read_to_string(&path).await {
        Ok(c) => c,
        Err(e) => return ToolOutput {
            text: format!("Failed to read {}: {}", path.display(), e),
            is_error: true,
            details: None,
        },
    };

    let offset = parse_u64(&input["offset"]).unwrap_or(1).max(1) as usize - 1;
    let limit = parse_u64(&input["limit"]).unwrap_or(2000) as usize;

    let lines: Vec<&str> = content.lines().collect();
    let total = lines.len();
    let end = (offset + limit).min(total);
    let selected = &lines[offset.min(total)..end];

    let numbered: String = selected.iter().enumerate()
        .map(|(i, line)| format!("{:>6}\t{}", offset + i + 1, line))
        .collect::<Vec<_>>()
        .join("\n");

    let text = if end < total {
        format!("{}\n\n({} total lines, showing {}-{})", numbered, total, offset + 1, end)
    } else {
        numbered
    };

    ToolOutput {
        text,
        is_error: false,
        details: Some(serde_json::json!({
            "path": path_str,
            "total_lines": total,
            "offset": offset + 1,
            "limit": limit,
        })),
    }
}

async fn run_write(input: serde_json::Value, cwd: PathBuf) -> ToolOutput {
    let path_str = match input["path"].as_str() {
        Some(p) => p,
        None => return ToolOutput {
            text: "missing 'path' parameter".into(),
            is_error: true,
            details: None,
        },
    };
    let content = match input["content"].as_str() {
        Some(c) => c,
        None => return ToolOutput {
            text: "missing 'content' parameter".into(),
            is_error: true,
            details: None,
        },
    };

    let path = resolve_path(path_str, &cwd);
    if let Some(parent) = path.parent() {
        if let Err(e) = tokio::fs::create_dir_all(parent).await {
            return ToolOutput {
                text: format!("Failed to create directories: {}", e),
                is_error: true,
                details: None,
            };
        }
    }

    match tokio::fs::write(&path, content).await {
        Ok(()) => ToolOutput {
            text: format!("Wrote {} bytes to {}", content.len(), path.display()),
            is_error: false,
            details: Some(serde_json::json!({
                "path": path_str,
                "size": content.len(),
            })),
        },
        Err(e) => ToolOutput {
            text: format!("Failed to write: {}", e),
            is_error: true,
            details: None,
        },
    }
}

async fn run_edit(input: serde_json::Value, cwd: PathBuf) -> ToolOutput {
    let path_str = match input["path"].as_str() {
        Some(p) => p,
        None => return ToolOutput {
            text: "missing 'path' parameter".into(),
            is_error: true,
            details: None,
        },
    };
    let old_text = match input["old_text"].as_str() {
        Some(t) => t,
        None => return ToolOutput {
            text: "missing 'old_text' parameter".into(),
            is_error: true,
            details: None,
        },
    };
    let new_text = match input["new_text"].as_str() {
        Some(t) => t,
        None => return ToolOutput {
            text: "missing 'new_text' parameter".into(),
            is_error: true,
            details: None,
        },
    };

    let path = resolve_path(path_str, &cwd);
    let content = match tokio::fs::read_to_string(&path).await {
        Ok(c) => c,
        Err(e) => return ToolOutput {
            text: format!("Failed to read {}: {}", path.display(), e),
            is_error: true,
            details: None,
        },
    };

    let count = content.matches(old_text).count();
    if count == 0 {
        return ToolOutput {
            text: "old_text not found in file".into(),
            is_error: true,
            details: None,
        };
    }
    if count > 1 {
        return ToolOutput {
            text: format!("old_text found {} times; must be unique", count),
            is_error: true,
            details: None,
        };
    }

    let new_content = content.replacen(old_text, new_text, 1);
    match tokio::fs::write(&path, &new_content).await {
        Ok(()) => ToolOutput {
            text: format!("Edited {}", path.display()),
            is_error: false,
            details: Some(serde_json::json!({
                "path": path_str,
                "old_text": old_text,
                "new_text": new_text,
            })),
        },
        Err(e) => ToolOutput {
            text: format!("Failed to write: {}", e),
            is_error: true,
            details: None,
        },
    }
}

fn resolve_path(path_str: &str, cwd: &Path) -> PathBuf {
    let path = Path::new(path_str);
    if path.is_absolute() { path.to_path_buf() } else { cwd.join(path) }
}

/// Parse a JSON value as u64, handling both number and string representations.
/// Models sometimes send integer parameters as strings (e.g. `"133"` instead of `133`).
fn parse_u64(v: &serde_json::Value) -> Option<u64> {
    v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok()))
}

fn truncate_output(output: &str, max_lines: usize, max_bytes: usize) -> String {
    if output.len() <= max_bytes {
        let lines: Vec<&str> = output.lines().collect();
        if lines.len() <= max_lines {
            return output.to_string();
        }
        return lines[..max_lines].join("\n")
            + &format!("\n... ({} lines truncated)", lines.len() - max_lines);
    }
    let truncated = &output[..max_bytes];
    format!("{}\n... (truncated, {} bytes total)", truncated, output.len())
}
