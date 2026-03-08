pub mod accumulator;
pub mod model;
pub mod provider;
pub mod store;
pub mod stream;

// Top-level re-exports for convenience.
pub use model::{
    gen_id, complete_tool_pairs, ContentBlock, Context, ContextId, Message, MessageId, Role,
    SessionId, Usage,
};
pub use stream::StreamEvent;
pub use provider::{
    ApiError, AuthMethod, EventStream, LlmProvider, Model, ModelCost, RequestOptions,
    ThinkingLevel, Tool, ToolContext, ToolOutput, ToolSchema,
};
pub use accumulator::StreamAccumulator;
pub use store::{Pool, Session, SessionHeader, Store};
