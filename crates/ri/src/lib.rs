pub mod accumulator;
pub mod model;
pub mod provider;
pub mod store;
pub mod stream;
pub mod tool;

// Top-level re-exports for convenience.
pub use model::{
    gen_id, gen_obj_id, complete_tool_pairs, ContentBlock, Context, ContextId, Message, MessageId, Role,
    SessionId, ThinkingReplay, Usage,
};
pub use stream::StreamEvent;
pub use provider::{
    ApiError, AuthMethod, EventStream, LlmProvider, Model, ModelCost, RequestOptions,
    ThinkingLevel,
};
pub use tool::{Tool, ToolContext, ToolOutput, ToolSchema};
pub use accumulator::StreamAccumulator;
pub use store::{Pool, Session, Store};
