use async_trait::async_trait;
use ri_core::tool::{Tool, ToolResultOutput, ToolUpdate};
use ri_core::types::{ContentBlock, ToolSchema};

pub struct ReadTool {
    cwd: String,
}

impl ReadTool {
    pub fn new(cwd: &str) -> Self {
        Self { cwd: cwd.to_string() }
    }
}

#[async_trait]
impl Tool for ReadTool {
    fn name(&self) -> &str { "read" }
    fn label(&self) -> &str { "Read" }
    fn description(&self) -> &str { "Read a file's contents" }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "read".to_string(),
            description: self.description().to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "File path to read" },
                    "offset": { "type": "integer", "description": "Starting line (0-based)" },
                    "limit": { "type": "integer", "description": "Number of lines to read" }
                },
                "required": ["path"]
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
        let path_str = params["path"]
            .as_str()
            .ok_or_else(|| eyre::eyre!("missing 'path' parameter"))?;

        let path = std::path::Path::new(path_str);
        let path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::path::Path::new(&self.cwd).join(path)
        };

        let content = tokio::fs::read_to_string(&path).await.map_err(|e| {
            eyre::eyre!("Failed to read {}: {}", path.display(), e)
        })?;

        let offset = params["offset"].as_u64().unwrap_or(0) as usize;
        let limit = params["limit"].as_u64().unwrap_or(2000) as usize;

        let lines: Vec<&str> = content.lines().collect();
        let total = lines.len();
        let end = (offset + limit).min(total);
        let selected = &lines[offset.min(total)..end];

        let numbered: String = selected
            .iter()
            .enumerate()
            .map(|(i, line)| format!("{:>6}\t{}", offset + i + 1, line))
            .collect::<Vec<_>>()
            .join("\n");

        let text = if end < total {
            format!(
                "{}\n\n({} total lines, showing {}-{})",
                numbered,
                total,
                offset + 1,
                end
            )
        } else {
            numbered
        };

        Ok(ToolResultOutput {
            content: vec![ContentBlock::text(text)],
            is_error: false,
        })
    }
}
