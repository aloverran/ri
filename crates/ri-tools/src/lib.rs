// Built-in tools for ri.
//
// Each tool is a function that returns a ToolDef.
// No trait objects, no Arc, no dynamic dispatch.

use std::path::{Path, PathBuf};

use ri::{ToolDef, ToolOutput};

pub fn all_tools() -> Vec<ToolDef> {
    vec![bash(), read(), write(), edit()]
}

pub fn bash() -> ToolDef {
    ToolDef {
        name: "bash",
        description: "Execute a shell command",
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "The shell command to execute" },
                "timeout": { "type": "integer", "description": "Timeout in milliseconds" }
            },
            "required": ["command"]
        }),
        run: |input, cwd, cancel| Box::pin(run_bash(input, cwd, cancel)),
    }
}

pub fn read() -> ToolDef {
    ToolDef {
        name: "read",
        description: "Read a file's contents",
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File path to read" },
                "offset": { "type": "integer", "description": "Starting line (1-indexed)" },
                "limit": { "type": "integer", "description": "Number of lines to read" }
            },
            "required": ["path"]
        }),
        run: |input, cwd, _cancel| Box::pin(run_read(input, cwd)),
    }
}

pub fn write() -> ToolDef {
    ToolDef {
        name: "write",
        description: "Write content to a file",
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File path to write" },
                "content": { "type": "string", "description": "Content to write" }
            },
            "required": ["path", "content"]
        }),
        run: |input, cwd, _cancel| Box::pin(run_write(input, cwd)),
    }
}

pub fn edit() -> ToolDef {
    ToolDef {
        name: "edit",
        description: "Replace text in a file (exact match)",
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "old_text": { "type": "string", "description": "Exact text to find and replace" },
                "new_text": { "type": "string", "description": "Replacement text" }
            },
            "required": ["path", "old_text", "new_text"]
        }),
        run: |input, cwd, _cancel| Box::pin(run_edit(input, cwd)),
    }
}

// -- Tool implementations --

async fn run_bash(
    input: serde_json::Value,
    cwd: PathBuf,
    cancel: tokio_util::sync::CancellationToken,
) -> ToolOutput {
    let command = match input["command"].as_str() {
        Some(c) => c,
        None => return ToolOutput { text: "missing 'command' parameter".into(), is_error: true },
    };
    let timeout_ms = input["timeout"].as_u64().unwrap_or(120_000);

    let mut cmd = tokio::process::Command::new("sh");
    cmd.arg("-c")
        .arg(command)
        .current_dir(&cwd)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    // Create a new process group so we can kill the entire tree on timeout.
    #[cfg(unix)]
    cmd.process_group(0);

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return ToolOutput { text: format!("Failed to spawn: {}", e), is_error: true },
    };

    let pid = child.id();

    // Take stdout/stderr handles for concurrent reading.
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

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

    // Wait for process with timeout and cancellation.
    let status = tokio::select! {
        result = child.wait() => Some(result),
        _ = tokio::time::sleep(std::time::Duration::from_millis(timeout_ms)) => None,
        _ = cancel.cancelled() => None,
    };

    if status.is_none() {
        // Kill the entire process group.
        #[cfg(unix)]
        if let Some(pid) = pid {
            unsafe { libc::kill((pid as i32).wrapping_neg(), libc::SIGKILL); }
        }
        // Fallback: kill the direct child.
        let _ = child.start_kill();
        // Reap to avoid zombie.
        let _ = child.wait().await;
        // Wait for pipe readers to finish.
        let _ = stdout_task.await;
        let _ = stderr_task.await;
        return ToolOutput {
            text: if cancel.is_cancelled() {
                "Command aborted".into()
            } else {
                format!("Command timed out after {}ms", timeout_ms)
            },
            is_error: true,
        };
    }

    let exit_status = match status.unwrap() {
        Ok(s) => s,
        Err(e) => {
            let _ = stdout_task.await;
            let _ = stderr_task.await;
            return ToolOutput { text: format!("Process error: {}", e), is_error: true };
        }
    };

    let stdout_bytes = stdout_task.await.unwrap_or_default();
    let stderr_bytes = stderr_task.await.unwrap_or_default();
    let exit_code = exit_status.code().unwrap_or(-1);

    let stdout = String::from_utf8_lossy(&stdout_bytes);
    let stderr = String::from_utf8_lossy(&stderr_bytes);

    let combined = if stderr.is_empty() {
        stdout.to_string()
    } else {
        format!("{}\n--- stderr ---\n{}", stdout, stderr)
    };

    let truncated = truncate_output(&combined, 2000, 50 * 1024);
    let text = format!("Exit code: {}\n{}", exit_code, truncated);

    ToolOutput { text, is_error: exit_code != 0 }
}

async fn run_read(input: serde_json::Value, cwd: PathBuf) -> ToolOutput {
    let path_str = match input["path"].as_str() {
        Some(p) => p,
        None => return ToolOutput { text: "missing 'path' parameter".into(), is_error: true },
    };

    let path = resolve_path(path_str, &cwd);
    let content = match tokio::fs::read_to_string(&path).await {
        Ok(c) => c,
        Err(e) => return ToolOutput { text: format!("Failed to read {}: {}", path.display(), e), is_error: true },
    };

    let offset = input["offset"].as_u64().unwrap_or(1).max(1) as usize - 1;
    let limit = input["limit"].as_u64().unwrap_or(2000) as usize;

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

    ToolOutput { text, is_error: false }
}

async fn run_write(input: serde_json::Value, cwd: PathBuf) -> ToolOutput {
    let path_str = match input["path"].as_str() {
        Some(p) => p,
        None => return ToolOutput { text: "missing 'path' parameter".into(), is_error: true },
    };
    let content = match input["content"].as_str() {
        Some(c) => c,
        None => return ToolOutput { text: "missing 'content' parameter".into(), is_error: true },
    };

    let path = resolve_path(path_str, &cwd);
    if let Some(parent) = path.parent() {
        if let Err(e) = tokio::fs::create_dir_all(parent).await {
            return ToolOutput { text: format!("Failed to create directories: {}", e), is_error: true };
        }
    }

    match tokio::fs::write(&path, content).await {
        Ok(()) => ToolOutput {
            text: format!("Wrote {} bytes to {}", content.len(), path.display()),
            is_error: false,
        },
        Err(e) => ToolOutput { text: format!("Failed to write: {}", e), is_error: true },
    }
}

async fn run_edit(input: serde_json::Value, cwd: PathBuf) -> ToolOutput {
    let path_str = match input["path"].as_str() {
        Some(p) => p,
        None => return ToolOutput { text: "missing 'path' parameter".into(), is_error: true },
    };
    let old_text = match input["old_text"].as_str() {
        Some(t) => t,
        None => return ToolOutput { text: "missing 'old_text' parameter".into(), is_error: true },
    };
    let new_text = match input["new_text"].as_str() {
        Some(t) => t,
        None => return ToolOutput { text: "missing 'new_text' parameter".into(), is_error: true },
    };

    let path = resolve_path(path_str, &cwd);
    let content = match tokio::fs::read_to_string(&path).await {
        Ok(c) => c,
        Err(e) => return ToolOutput { text: format!("Failed to read {}: {}", path.display(), e), is_error: true },
    };

    let count = content.matches(old_text).count();
    if count == 0 {
        return ToolOutput { text: "old_text not found in file".into(), is_error: true };
    }
    if count > 1 {
        return ToolOutput {
            text: format!("old_text found {} times; must be unique", count),
            is_error: true,
        };
    }

    let new_content = content.replacen(old_text, new_text, 1);
    match tokio::fs::write(&path, &new_content).await {
        Ok(()) => ToolOutput { text: format!("Edited {}", path.display()), is_error: false },
        Err(e) => ToolOutput { text: format!("Failed to write: {}", e), is_error: true },
    }
}

fn resolve_path(path_str: &str, cwd: &Path) -> PathBuf {
    let path = Path::new(path_str);
    if path.is_absolute() { path.to_path_buf() } else { cwd.join(path) }
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
