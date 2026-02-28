pub mod accumulator;
pub mod message;
pub mod provider;
pub mod store;
pub mod stream;

// Top-level re-exports for convenience.
pub use message::{
    gen_id, ContentBlock, Message, MessageId, Role, SessionId, StepId, Usage,
};
pub use stream::StreamEvent;
pub use provider::{
    ApiError, AuthMethod, EventStream, LlmProvider, Model, ModelCost, RequestOptions,
    ThinkingLevel, Tool, ToolContext, ToolOutput, ToolSchema,
};
pub use accumulator::StreamAccumulator;
pub use store::{Context, Pool, Session, SessionHeader, Step, Store};
