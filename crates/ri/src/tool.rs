//! Tool definitions -- plain functions, not trait objects.

use std::path::PathBuf;
use std::pin::Pin;
use std::future::Future;

/// Tool schema sent to the LLM API so it knows what tools are available.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ToolSchema {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

/// Result of executing a tool. Text is returned to the LLM as a tool_result content block.
pub struct ToolOutput {
    pub text: String,
    pub is_error: bool,
}

/// Function pointer type for tool implementations. Each tool is a plain fn that
/// returns a boxed future (needed to store heterogeneous async fns in a Vec).
pub type ToolFn = fn(
    serde_json::Value,
    PathBuf,
    tokio_util::sync::CancellationToken,
) -> Pin<Box<dyn Future<Output = ToolOutput> + Send>>;

/// Static tool definition: schema for the LLM + implementation function.
pub struct ToolDef {
    pub name: &'static str,
    pub description: &'static str,
    pub parameters: serde_json::Value,
    pub run: ToolFn,
}

impl ToolDef {
    pub fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.name.to_string(),
            description: self.description.to_string(),
            parameters: self.parameters.clone(),
        }
    }
}
