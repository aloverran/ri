// Agent event system.
//
// The agent loop emits typed events that consumers (TUI, RPC, print mode)
// can subscribe to. Uses tokio::sync::broadcast for fan-out.

use crate::types::{ContentBlock, ToolCall};

#[derive(Debug, Clone)]
pub enum AgentEvent {
    AgentStart,
    AgentEnd,
    TurnStart,
    TurnEnd,
    MessageStart,
    MessageUpdate(AssistantStreamEvent),
    MessageEnd,
    ToolExecutionStart {
        tool_call: ToolCall,
    },
    ToolExecutionUpdate {
        tool_call_id: String,
        partial: Vec<ContentBlock>,
    },
    ToolExecutionEnd {
        tool_call_id: String,
        result: Vec<ContentBlock>,
        is_error: bool,
    },
}

#[derive(Debug, Clone)]
pub enum AssistantStreamEvent {
    TextStart,
    TextDelta { delta: String },
    TextEnd { signature: Option<String> },
    ThinkingStart,
    ThinkingDelta { delta: String },
    ThinkingEnd { signature: Option<String> },
    ToolCallStart { id: String, name: String },
    ToolCallDelta { id: String, delta: String },
    ToolCallEnd { id: String, signature: Option<String> },
    Done,
    Error { message: String },
}

pub type EventSender = tokio::sync::broadcast::Sender<AgentEvent>;
pub type EventReceiver = tokio::sync::broadcast::Receiver<AgentEvent>;

pub fn event_channel(capacity: usize) -> (EventSender, EventReceiver) {
    tokio::sync::broadcast::channel(capacity)
}
