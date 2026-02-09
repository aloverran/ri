// Tool trait -- the interface that all built-in and custom tools implement.

use async_trait::async_trait;
use crate::types::{ContentBlock, ToolSchema};

#[derive(Debug, Clone)]
pub struct ToolResultOutput {
    pub content: Vec<ContentBlock>,
    pub is_error: bool,
}

/// Update sent during long-running tool execution.
#[derive(Debug, Clone)]
pub struct ToolUpdate {
    pub content: Vec<ContentBlock>,
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn label(&self) -> &str;
    fn description(&self) -> &str;
    fn schema(&self) -> ToolSchema;

    async fn execute(
        &self,
        tool_call_id: &str,
        params: serde_json::Value,
        cancel: tokio_util::sync::CancellationToken,
        update_tx: Option<tokio::sync::mpsc::Sender<ToolUpdate>>,
    ) -> eyre::Result<ToolResultOutput>;
}
