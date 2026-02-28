use std::collections::HashMap;

pub mod accumulator;
pub mod message;
pub mod types;
pub mod store;

/// Bag of extra JSON fields preserved for round-trip / forward-compat.
pub type JsonMap = HashMap<String, serde_json::Value>;

// Top-level re-exports for convenience.
pub use message::{
    gen_id, ContentBlock, Context, Message, MessageId, Pool, Role, SessionId, Step, StepId, Usage,
};
pub use types::{
    ApiError, AuthMethod, EventStream, LlmProvider, Model, ModelCost, RequestOptions,
    StreamEvent, ThinkingLevel, Tool, ToolContext, ToolOutput, ToolSchema,
};
pub use accumulator::StreamAccumulator;
pub use store::{Store, Session, SessionHeader};
