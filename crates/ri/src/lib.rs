use std::collections::HashMap;

pub mod event;
pub mod id;
pub mod message;
pub mod model;
pub mod provider;
pub mod filing;
pub mod tool;

/// Bag of extra JSON fields preserved for round-trip / forward-compat.
pub type JsonMap = HashMap<String, serde_json::Value>;

// Top-level re-exports for convenience.
pub use event::StreamEvent;
pub use id::gen_id;
pub use message::{
    ContentBlock, Message, MessagePool, Provenance, Role, Usage,
};
pub use model::{Model, ModelCost, ThinkingLevel};
pub use provider::{ApiError, AuthMethod, EventStream, LlmProvider, RequestOptions};
pub use filing::{SessionStore, SessionHeader};
pub use tool::{ToolDef, ToolFn, ToolOutput, ToolSchema};
