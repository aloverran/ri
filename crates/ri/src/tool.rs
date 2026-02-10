// Tools -- functions, not trait objects.

use std::path::PathBuf;
use std::pin::Pin;
use std::future::Future;

// Tool schema as seen by the LLM API.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ToolSchema {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

pub struct ToolOutput {
    pub text: String,
    pub is_error: bool,
}

pub type ToolFn = fn(
    serde_json::Value,
    PathBuf,
    tokio_util::sync::CancellationToken,
) -> Pin<Box<dyn Future<Output = ToolOutput> + Send>>;

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
