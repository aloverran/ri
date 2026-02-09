use async_trait::async_trait;
use ri_core::tool::{Tool, ToolResultOutput, ToolUpdate};
use ri_core::types::{ContentBlock, ToolSchema};

pub struct BashTool {
    cwd: String,
}

impl BashTool {
    pub fn new(cwd: &str) -> Self {
        Self {
            cwd: cwd.to_string(),
        }
    }
}

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }

    fn label(&self) -> &str {
        "Bash"
    }

    fn description(&self) -> &str {
        "Execute a shell command"
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "bash".to_string(),
            description: self.description().to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "The shell command to execute"
                    },
                    "timeout": {
                        "type": "integer",
                        "description": "Timeout in milliseconds"
                    }
                },
                "required": ["command"]
            }),
        }
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: serde_json::Value,
        cancel: tokio_util::sync::CancellationToken,
        update_tx: Option<tokio::sync::mpsc::Sender<ToolUpdate>>,
    ) -> eyre::Result<ToolResultOutput> {
        let command = params["command"]
            .as_str()
            .ok_or_else(|| eyre::eyre!("missing 'command' parameter"))?;

        let timeout_ms = params["timeout"].as_u64().unwrap_or(120_000);

        // Spawn shell process
        let mut child = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(command)
            .current_dir(&self.cwd)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()?;

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        let _ = (stdout, stderr, update_tx);

        // Read output with timeout and cancellation
        let output = tokio::select! {
            result = child.wait_with_output() => result?,
            _ = tokio::time::sleep(std::time::Duration::from_millis(timeout_ms)) => {
                // child is consumed by wait_with_output branch, so this won't run
                // if wait_with_output wins. If timeout wins, child is still alive.
                return Ok(ToolResultOutput {
                    content: vec![ContentBlock::text(format!("Command timed out after {}ms", timeout_ms))],
                    is_error: true,
                });
            }
            _ = cancel.cancelled() => {
                return Ok(ToolResultOutput {
                    content: vec![ContentBlock::text("Command aborted")],
                    is_error: true,
                });
            }
        };

        let stdout_str = String::from_utf8_lossy(&output.stdout);
        let stderr_str = String::from_utf8_lossy(&output.stderr);
        let exit_code = output.status.code().unwrap_or(-1);

        // Truncate if needed (2000 lines or 50KB)
        let combined = if stderr_str.is_empty() {
            stdout_str.to_string()
        } else {
            format!("{}\n--- stderr ---\n{}", stdout_str, stderr_str)
        };

        let truncated = truncate_output(&combined, 2000, 50 * 1024);

        let text = format!(
            "Exit code: {}\n{}",
            exit_code, truncated
        );

        Ok(ToolResultOutput {
            content: vec![ContentBlock::text(text)],
            is_error: exit_code != 0,
        })
    }
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
    format!(
        "{}\n... (truncated, {} bytes total)",
        truncated,
        output.len()
    )
}
