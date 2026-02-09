use async_trait::async_trait;
use ri_core::tool::{Tool, ToolResultOutput, ToolUpdate};
use ri_core::types::{ContentBlock, ToolSchema};

pub struct FindTool {
    cwd: String,
}

impl FindTool {
    pub fn new(cwd: &str) -> Self {
        Self { cwd: cwd.to_string() }
    }
}

#[async_trait]
impl Tool for FindTool {
    fn name(&self) -> &str { "find" }
    fn label(&self) -> &str { "Find" }
    fn description(&self) -> &str { "Find files matching a glob pattern" }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "find".to_string(),
            description: self.description().to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Glob pattern (e.g. **/*.rs)" },
                    "path": { "type": "string", "description": "Directory to search in" }
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
        let search_dir = params["path"].as_str().unwrap_or(&self.cwd);

        let full_pattern = format!("{}/{}", search_dir, pattern);
        let paths: Vec<String> = glob::glob(&full_pattern)?
            .filter_map(|entry| entry.ok())
            .map(|p| p.display().to_string())
            .collect();

        let text = if paths.is_empty() {
            "No files found".to_string()
        } else {
            paths.join("\n")
        };

        Ok(ToolResultOutput {
            content: vec![ContentBlock::Text { text }],
            is_error: false,
        })
    }
}
