use async_trait::async_trait;
use ri_core::tool::{Tool, ToolResultOutput, ToolUpdate};
use ri_core::types::{ContentBlock, ToolSchema};

pub struct WriteTool {
    cwd: String,
}

impl WriteTool {
    pub fn new(cwd: &str) -> Self {
        Self { cwd: cwd.to_string() }
    }
}

#[async_trait]
impl Tool for WriteTool {
    fn name(&self) -> &str { "write" }
    fn label(&self) -> &str { "Write" }
    fn description(&self) -> &str { "Write content to a file" }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "write".to_string(),
            description: self.description().to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "File path to write" },
                    "content": { "type": "string", "description": "Content to write" }
                },
                "required": ["path", "content"]
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
        let path_str = params["path"].as_str()
            .ok_or_else(|| eyre::eyre!("missing 'path'"))?;
        let content = params["content"].as_str()
            .ok_or_else(|| eyre::eyre!("missing 'content'"))?;

        let path = std::path::Path::new(path_str);
        let path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::path::Path::new(&self.cwd).join(path)
        };

        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        tokio::fs::write(&path, content).await?;

        Ok(ToolResultOutput {
            content: vec![ContentBlock::Text {
                text: format!("Wrote {} bytes to {}", content.len(), path.display()),
            }],
            is_error: false,
        })
    }
}
