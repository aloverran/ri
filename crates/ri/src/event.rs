// Normalized stream events, shared across providers.
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
