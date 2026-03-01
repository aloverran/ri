//! Streaming protocol for LLM responses.
//!
//! `StreamEvent` is the normalized incremental form of content blocks as they
//! arrive from a provider. A TextStart/Delta/End sequence accumulates into a
//! `ContentBlock::Text`, and so on. The `StreamAccumulator` handles this
//! conversion.

use crate::model::Usage;

/// Normalized event emitted by LLM providers during response streaming.
#[derive(Debug, Clone)]
pub enum StreamEvent {
    TextStart,
    TextDelta(String),
    TextEnd { sig: Option<String> },
    ThinkingStart,
    ThinkingDelta(String),
    ThinkingEnd { sig: Option<String> },
    ToolCallStart { id: String, name: String },
    ToolCallDelta { id: String, json_fragment: String },
    ToolCallEnd { id: String, sig: Option<String> },
    Usage(Usage),
    Done,
    Error(String),
}
