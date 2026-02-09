use async_trait::async_trait;
use ri_core::tool::{Tool, ToolResultOutput, ToolUpdate};
use ri_core::types::{ContentBlock, ToolSchema};

pub struct GrepTool {
    cwd: String,
}

impl GrepTool {
    pub fn new(cwd: &str) -> Self {
        Self { cwd: cwd.to_string() }
    }
}

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &str { "grep" }
    fn label(&self) -> &str { "Grep" }
    fn description(&self) -> &str { "Search file contents with regex" }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "grep".to_string(),
            description: self.description().to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Regex pattern" },
                    "path": { "type": "string", "description": "Directory or file to search" },
                    "include": { "type": "string", "description": "File glob filter (e.g. *.rs)" }
                },
                "required": ["pattern"]
            }),
        }
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: serde_json::Value,
        _cancel: tokio_util::sync::CancellationToken,
        _update_tx: Option<tokio::sync::mpsc::Sender<ToolUpdate>>,
    ) -> eyre::Result<ToolResultOutput> {
        let pattern = params["pattern"].as_str()
            .ok_or_else(|| eyre::eyre!("missing 'pattern'"))?;
        let search_path = params["path"].as_str().unwrap_or(&self.cwd);
        let include = params["include"].as_str();

        // Use ripgrep if available, fall back to grep
        let mut cmd = tokio::process::Command::new("rg");
        cmd.arg("--line-number")
            .arg("--no-heading")
            .arg("--color=never")
            .arg(pattern)
            .arg(search_path);

        if let Some(glob) = include {
            cmd.arg("--glob").arg(glob);
        }

        let output = cmd.output().await;

        let result = match output {
            Ok(out) if out.status.success() => {
                String::from_utf8_lossy(&out.stdout).to_string()
            }
            Ok(out) if out.status.code() == Some(1) => {
                "No matches found".to_string()
            }
            Ok(out) => {
                format!("grep error: {}", String::from_utf8_lossy(&out.stderr))
            }
            Err(_) => {
                // ripgrep not available, try grep
                let out = tokio::process::Command::new("grep")
                    .arg("-rn")
                    .arg(pattern)
                    .arg(search_path)
                    .output()
                    .await?;
                String::from_utf8_lossy(&out.stdout).to_string()
            }
        };

        Ok(ToolResultOutput {
            content: vec![ContentBlock::text(result)],
            is_error: false,
        })
    }
}
