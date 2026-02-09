use async_trait::async_trait;
use ri_core::tool::{Tool, ToolResultOutput, ToolUpdate};
use ri_core::types::{ContentBlock, ToolSchema};

pub struct LsTool {
    cwd: String,
}

impl LsTool {
    pub fn new(cwd: &str) -> Self {
        Self { cwd: cwd.to_string() }
    }
}

#[async_trait]
impl Tool for LsTool {
    fn name(&self) -> &str { "ls" }
    fn label(&self) -> &str { "List" }
    fn description(&self) -> &str { "List directory contents" }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "ls".to_string(),
            description: self.description().to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Directory to list" }
                }
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
        let dir = params["path"].as_str().unwrap_or(&self.cwd);
        let path = std::path::Path::new(dir);
        let path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::path::Path::new(&self.cwd).join(path)
        };

        let mut entries = tokio::fs::read_dir(&path).await?;
        let mut lines = Vec::new();

        while let Some(entry) = entries.next_entry().await? {
            let meta = entry.metadata().await?;
            let name = entry.file_name().to_string_lossy().to_string();
            let kind = if meta.is_dir() { "dir " } else { "file" };
            let size = if meta.is_file() {
                format!("{:>8}", meta.len())
            } else {
                "       -".to_string()
            };
            lines.push(format!("{} {} {}", kind, size, name));
        }

        lines.sort();

        let text = if lines.is_empty() {
            "(empty directory)".to_string()
        } else {
            lines.join("\n")
        };

        Ok(ToolResultOutput {
            content: vec![ContentBlock::text(text)],
            is_error: false,
        })
    }
}
