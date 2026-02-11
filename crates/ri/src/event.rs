/// Normalized stream events emitted by LLM providers during response streaming.
/// The agent loop consumes these to accumulate content blocks and detect tool calls.
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
    Done,
    Error(String),
}
