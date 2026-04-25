pub mod accumulator;
pub mod model;
pub mod provider;
pub mod store;
pub mod stream;
pub mod tool;

// Top-level re-exports for convenience.
pub use model::{
    complete_tool_pairs, ContentBlock, Context, ContextId, Facet, HasMeta, Message, MessageId,
    Ref, RefId, Role, ThinkingReplay, Usage,
};
pub use stream::StreamEvent;
pub use provider::{
    ApiError, AuthMethod, EventStream, LlmProvider, Model, ModelCost, RequestOptions,
    ThinkingLevel,
};
pub use tool::{Tool, ToolOutput, ToolSchema};
pub use accumulator::StreamAccumulator;
pub use store::{default_sessions_dir, MountId, Pool, Store};
