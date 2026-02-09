use async_trait::async_trait;
use ri_core::tool::{Tool, ToolResultOutput, ToolUpdate};
use ri_core::types::{ContentBlock, ToolSchema};

pub struct EditTool {
    cwd: String,
}

impl EditTool {
    pub fn new(cwd: &str) -> Self {
        Self { cwd: cwd.to_string() }
    }
}

#[async_trait]
impl Tool for EditTool {
    fn name(&self) -> &str { "edit" }
    fn label(&self) -> &str { "Edit" }
    fn description(&self) -> &str { "Replace text in a file (exact match)" }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "edit".to_string(),
            description: self.description().to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "old_text": { "type": "string", "description": "Exact text to find and replace" },
                    "new_text": { "type": "string", "description": "Replacement text" }
                },
                "required": ["path", "old_text", "new_text"]
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
        let old_text = params["old_text"].as_str()
            .ok_or_else(|| eyre::eyre!("missing 'old_text'"))?;
        let new_text = params["new_text"].as_str()
            .ok_or_else(|| eyre::eyre!("missing 'new_text'"))?;

        let path = std::path::Path::new(path_str);
        let path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::path::Path::new(&self.cwd).join(path)
        };

        let content = tokio::fs::read_to_string(&path).await?;

        let count = content.matches(old_text).count();
        if count == 0 {
            return Ok(ToolResultOutput {
                content: vec![ContentBlock::text("old_text not found in file")],
                is_error: true,
            });
        }
        if count > 1 {
            return Ok(ToolResultOutput {
                content: vec![ContentBlock::text(format!("old_text found {} times; must be unique", count))],
                is_error: true,
            });
        }

        let new_content = content.replacen(old_text, new_text, 1);
        tokio::fs::write(&path, &new_content).await?;

        Ok(ToolResultOutput {
            content: vec![ContentBlock::text(format!("Edited {}", path.display()))],
            is_error: false,
        })
    }
}
