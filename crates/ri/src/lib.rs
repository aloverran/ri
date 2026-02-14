use std::collections::HashMap;

pub mod message;
pub mod provider;
pub mod filing;

/// Bag of extra JSON fields preserved for round-trip / forward-compat.
pub type JsonMap = HashMap<String, serde_json::Value>;

// Top-level re-exports for convenience.
pub use message::{
    gen_id, ContentBlock, Message, MessagePool, Provenance, Role, Usage,
};
pub use provider::{
    ApiError, AuthMethod, EventStream, LlmProvider, Model, ModelCost, RequestOptions,
    StreamEvent, ThinkingLevel, ToolDef, ToolFn, ToolOutput, ToolSchema,
};
pub use filing::{SessionStore, SessionHeader};
