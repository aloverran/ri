//! Accumulates StreamEvents into ContentBlocks.
//!
//! Pure state machine with no I/O. Feed it stream events as they arrive;
//! call finish() when the stream ends to get the completed content blocks
//! and usage.

use std::collections::HashMap;

use crate::{ContentBlock, StreamEvent, Usage};

/// In-progress tool call being assembled from streaming deltas.
struct PendingToolCall {
    name: String,
    json_buf: String,
}

/// Accumulates streaming LLM events into completed content blocks.
///
/// Used internally by `ri_ai::Turn`, but also available directly for
/// custom streaming scenarios that bypass `Turn`.
pub struct StreamAccumulator {
    text_buf: String,
    thinking_buf: String,
    tool_calls: HashMap<String, PendingToolCall>,
    content: Vec<ContentBlock>,
    usage: Option<Usage>,
}

impl StreamAccumulator {
    pub fn new() -> Self {
        Self {
            text_buf: String::new(),
            thinking_buf: String::new(),
            tool_calls: HashMap::new(),
            content: Vec::new(),
            usage: None,
        }
    }

    /// Feed a stream event into the accumulator.
    pub fn feed(&mut self, event: &StreamEvent) {
        match event {
            StreamEvent::TextStart => { self.text_buf.clear(); }
            StreamEvent::TextDelta(d) => { self.text_buf.push_str(d); }
            StreamEvent::TextEnd { .. } => {
                if !self.text_buf.is_empty() {
                    self.content.push(ContentBlock::Text {
                        text: std::mem::take(&mut self.text_buf),
                    });
                }
            }
            StreamEvent::ThinkingStart => { self.thinking_buf.clear(); }
            StreamEvent::ThinkingDelta(d) => { self.thinking_buf.push_str(d); }
            StreamEvent::ThinkingEnd { sig } => {
                if !self.thinking_buf.is_empty() {
                    self.content.push(ContentBlock::Thinking {
                        thinking: std::mem::take(&mut self.thinking_buf),
                        sig: sig.clone(),
                    });
                }
            }
            StreamEvent::ToolCallStart { id, name } => {
                self.tool_calls.insert(id.clone(), PendingToolCall {
                    name: name.clone(),
                    json_buf: String::new(),
                });
            }
            StreamEvent::ToolCallDelta { id, json_fragment } => {
                if let Some(tc) = self.tool_calls.get_mut(id) {
                    tc.json_buf.push_str(json_fragment);
                }
            }
            StreamEvent::ToolCallEnd { id, .. } => {
                if let Some(tc) = self.tool_calls.remove(id) {
                    let input: serde_json::Value = serde_json::from_str(&tc.json_buf)
                        .unwrap_or_else(|_| serde_json::json!({
                            "error": "Invalid JSON from model",
                            "partial": tc.json_buf,
                        }));
                    self.content.push(ContentBlock::ToolUse {
                        id: id.clone(),
                        name: tc.name,
                        input,
                    });
                }
            }
            StreamEvent::Usage(u) => { self.usage = Some(u.clone()); }
            StreamEvent::Error(e) => {
                self.content.push(ContentBlock::error(e));
            }
            StreamEvent::Done => {}
        }
    }

    /// Consume the accumulator, flushing any incomplete buffers,
    /// and return the completed content blocks and usage.
    pub fn finish(mut self) -> (Vec<ContentBlock>, Option<Usage>) {
        if !self.text_buf.is_empty() {
            self.content.push(ContentBlock::text(self.text_buf));
        }
        if !self.thinking_buf.is_empty() {
            self.content.push(ContentBlock::thinking(self.thinking_buf));
        }
        for (id, tc) in self.tool_calls {
            let input = serde_json::from_str(&tc.json_buf)
                .unwrap_or_else(|_| serde_json::json!({
                    "error": "Interrupted",
                    "partial": tc.json_buf,
                }));
            self.content.push(ContentBlock::tool_use(id, tc.name, input));
        }
        (self.content, self.usage)
    }
}

impl Default for StreamAccumulator {
    fn default() -> Self {
        Self::new()
    }
}
