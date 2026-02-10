// Re-export store types as canonical.
pub use ri_store::types::{
    Role, ContentBlock, Message, Provenance, Usage,
    SessionHeader, SessionInfo,
};
pub use ri_store::pool::Pool;
pub use ri_store::filing::SessionFiling;
pub use ri_store::id::gen_id;

use serde::{Deserialize, Serialize};

// Model definition (provider-level).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Model {
    pub id: String,
    pub name: String,
    pub reasoning: bool,
    pub context_window: usize,
    pub max_tokens: usize,
    pub cost: ModelCost,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelCost {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingLevel {
    Off,
    Low,
    Medium,
    High,
    #[serde(rename = "xhigh")]
    XHigh,
}
