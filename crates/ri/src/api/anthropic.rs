// Anthropic Messages API -- request building and SSE interpretation.
//
// Two entry points:
//   build_request() -> ApiRequest    (pure data)
//   event_stream()  -> EventStream   (SSE bytes -> typed events)

use std::collections::VecDeque;
use std::pin::Pin;
use std::task::{Context, Poll};
use futures::Stream;
use serde_json::{json, Value};
use tracing::warn;

use crate::types::*;
use super::{ApiError, ApiRequest, EventStream, RequestOptions, StreamEvent, ToolSchema};
use super::sse::{SseEvent, SseParser};

// -- Tool name mapping for OAuth (Claude Code compatibility) --

const CLAUDE_CODE_TOOLS: &[&str] = &[
    "Read", "Write", "Edit", "Bash", "Grep", "Glob",
    "AskUserQuestion", "EnterPlanMode", "ExitPlanMode",
    "KillShell", "NotebookEdit", "Skill", "Task",
    "TaskOutput", "TodoWrite", "WebFetch", "WebSearch",
];

fn to_claude_code_name(name: &str) -> String {
    CLAUDE_CODE_TOOLS.iter()
        .find(|cc| cc.eq_ignore_ascii_case(name))
        .map(|cc| cc.to_string())
        .unwrap_or_else(|| name.to_string())
}

fn from_claude_code_name(name: &str, original_tools: &[ToolSchema]) -> String {
    original_tools.iter()
        .find(|t| t.name.eq_ignore_ascii_case(name))
        .map(|t| t.name.clone())
        .unwrap_or_else(|| name.to_string())
}

// -- Request building --

pub fn build_request(api_key: &str, opts: &RequestOptions) -> ApiRequest {
    let is_oauth = api_key.starts_with("sk-ant-oat");
    let body = build_body(opts, is_oauth);
    let url = "https://api.anthropic.com/v1/messages".to_string();

    let mut headers = vec![
        ("anthropic-version".into(), "2023-06-01".into()),
        ("content-type".into(), "application/json".into()),
        ("accept".into(), "text/event-stream".into()),
    ];

    if is_oauth {
        headers.push(("authorization".into(), format!("Bearer {}", api_key)));
        headers.push((
            "anthropic-beta".into(),
            "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14,fine-grained-tool-streaming-2025-05-14".into(),
        ));
        headers.push(("anthropic-dangerous-direct-browser-access".into(), "true".into()));
        headers.push(("user-agent".into(), "claude-cli/2.1.2 (external, cli)".into()));
        headers.push(("x-app".into(), "cli".into()));
    } else {
        headers.push(("x-api-key".into(), api_key.to_string()));
        headers.push((
            "anthropic-beta".into(),
            "interleaved-thinking-2025-05-14,fine-grained-tool-streaming-2025-05-14".into(),
        ));
    }

    ApiRequest { url, headers, body: body.to_string() }
}

fn build_body(opts: &RequestOptions, is_oauth: bool) -> Value {
    let messages: Vec<Value> = opts.messages.iter()
        .filter(|m| m.role != Role::System)
        .map(|m| convert_message(m))
        .collect();

    let max_tokens = opts.max_tokens
        .unwrap_or_else(|| (opts.model.max_tokens / 3).max(4096));

    let mut body = json!({
        "model": opts.model.id,
        "messages": messages,
        "max_tokens": max_tokens,
        "stream": true,
    });

    // System prompt
    if is_oauth {
        let mut system_blocks = vec![json!({
            "type": "text",
            "text": "You are Claude Code, Anthropic's official CLI for Claude.",
            "cache_control": { "type": "ephemeral" }
        })];
        system_blocks.push(json!({
            "type": "text",
            "text": opts.system_prompt,
            "cache_control": { "type": "ephemeral" }
        }));
        body["system"] = json!(system_blocks);
    } else {
        body["system"] = json!([{
            "type": "text",
            "text": opts.system_prompt,
            "cache_control": { "type": "ephemeral" }
        }]);
    }

    // Tools
    if !opts.tools.is_empty() {
        let tools: Vec<Value> = opts.tools.iter().map(|t| {
            let name = if is_oauth { to_claude_code_name(&t.name) } else { t.name.clone() };
            json!({
                "name": name,
                "description": t.description,
                "input_schema": {
                    "type": "object",
                    "properties": t.parameters.get("properties").unwrap_or(&json!({})),
                    "required": t.parameters.get("required").unwrap_or(&json!([]))
                }
            })
        }).collect();
        body["tools"] = json!(tools);
    }

    // Thinking
    if opts.model.reasoning {
        if opts.thinking != ThinkingLevel::Off {
            let is_opus_46 = opts.model.id.contains("opus-4-6") || opts.model.id.contains("opus-4.6");
            if is_opus_46 {
                body["thinking"] = json!({ "type": "adaptive" });
                let effort = match opts.thinking {
                    ThinkingLevel::Low => "low",
                    ThinkingLevel::Medium => "medium",
                    ThinkingLevel::High => "high",
                    ThinkingLevel::XHigh => "max",
                    ThinkingLevel::Off => unreachable!(),
                };
                body["output_config"] = json!({ "effort": effort });
            } else {
                let budget = match opts.thinking {
                    ThinkingLevel::Low => 1024,
                    ThinkingLevel::Medium => 4096,
                    ThinkingLevel::High => 16384,
                    ThinkingLevel::XHigh => 32768,
                    ThinkingLevel::Off => unreachable!(),
                };
                body["thinking"] = json!({ "type": "enabled", "budget_tokens": budget });
            }
        }
    }

    body
}

fn convert_message(msg: &Message) -> Value {
    let role = match msg.role {
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::System => "user",
    };

    let content: Vec<Value> = msg.content.iter().map(convert_content).collect();
    let has_tool_results = msg.content.iter().any(|c| matches!(c, Content::ToolResult { .. }));
    let effective_role = if has_tool_results { "user" } else { role };

    json!({ "role": effective_role, "content": content })
}

fn convert_content(c: &Content) -> Value {
    match c {
        Content::Text { text, .. } => json!({ "type": "text", "text": text }),
        Content::Thinking { text, .. } => json!({ "type": "thinking", "thinking": text }),
        Content::Image { media_type, data } => json!({
            "type": "image",
            "source": { "type": "base64", "media_type": media_type, "data": data }
        }),
        Content::ToolUse { id, name, input, .. } => json!({
            "type": "tool_use", "id": id, "name": name, "input": input
        }),
        Content::ToolResult { tool_use_id, output, is_error } => json!({
            "type": "tool_result",
            "tool_use_id": tool_use_id,
            "content": [{ "type": "text", "text": output }],
            "is_error": is_error,
        }),
    }
}

// -- SSE interpretation --

#[derive(Debug)]
enum BlockKind {
    Text,
    Thinking,
    ToolUse { id: String },
}

pub struct StreamState {
    blocks: Vec<BlockKind>,
}

impl StreamState {
    fn new() -> Self {
        Self { blocks: Vec::new() }
    }

    fn interpret(&mut self, sse: &SseEvent) -> Vec<Result<StreamEvent, ApiError>> {
        let mut out = Vec::new();

        match sse.event_type.as_str() {
            "content_block_start" => {
                let parsed: Value = match serde_json::from_str(&sse.data) {
                    Ok(v) => v,
                    Err(e) => {
                        out.push(Err(ApiError::StreamParse(format!("content_block_start: {}", e))));
                        return out;
                    }
                };
                let block_type = parsed["content_block"]["type"].as_str().unwrap_or("");
                let index = parsed["index"].as_u64().unwrap_or(0) as usize;

                match block_type {
                    "text" => {
                        while self.blocks.len() <= index { self.blocks.push(BlockKind::Text); }
                        self.blocks[index] = BlockKind::Text;
                        out.push(Ok(StreamEvent::TextStart));
                    }
                    "thinking" => {
                        while self.blocks.len() <= index { self.blocks.push(BlockKind::Text); }
                        self.blocks[index] = BlockKind::Thinking;
                        out.push(Ok(StreamEvent::ThinkingStart));
                    }
                    "tool_use" => {
                        let id = parsed["content_block"]["id"].as_str().unwrap_or("").to_string();
                        let name = parsed["content_block"]["name"].as_str().unwrap_or("").to_string();
                        while self.blocks.len() <= index { self.blocks.push(BlockKind::Text); }
                        self.blocks[index] = BlockKind::ToolUse { id: id.clone() };
                        out.push(Ok(StreamEvent::ToolCallStart { id, name }));
                    }
                    other => { warn!("Unknown content block type: {}", other); }
                }
            }

            "content_block_delta" => {
                let parsed: Value = match serde_json::from_str(&sse.data) {
                    Ok(v) => v,
                    Err(e) => {
                        out.push(Err(ApiError::StreamParse(format!("content_block_delta: {}", e))));
                        return out;
                    }
                };
                let index = parsed["index"].as_u64().unwrap_or(0) as usize;
                let delta_type = parsed["delta"]["type"].as_str().unwrap_or("");

                match delta_type {
                    "text_delta" => {
                        let text = parsed["delta"]["text"].as_str().unwrap_or("").to_string();
                        out.push(Ok(StreamEvent::TextDelta(text)));
                    }
                    "thinking_delta" => {
                        let text = parsed["delta"]["thinking"].as_str().unwrap_or("").to_string();
                        out.push(Ok(StreamEvent::ThinkingDelta(text)));
                    }
                    "input_json_delta" => {
                        let partial = parsed["delta"]["partial_json"].as_str().unwrap_or("").to_string();
                        if let Some(BlockKind::ToolUse { id }) = self.blocks.get(index) {
                            out.push(Ok(StreamEvent::ToolCallDelta {
                                id: id.clone(),
                                json_fragment: partial,
                            }));
                        }
                    }
                    "signature_delta" => {} // thinking block signature, handled at block stop
                    other => { warn!("Unknown delta type: {}", other); }
                }
            }

            "content_block_stop" => {
                let parsed: Value = match serde_json::from_str(&sse.data) {
                    Ok(v) => v,
                    Err(e) => {
                        out.push(Err(ApiError::StreamParse(format!("content_block_stop: {}", e))));
                        return out;
                    }
                };
                let index = parsed["index"].as_u64().unwrap_or(0) as usize;
                if let Some(block) = self.blocks.get(index) {
                    match block {
                        BlockKind::Text => out.push(Ok(StreamEvent::TextEnd { sig: None })),
                        BlockKind::Thinking => out.push(Ok(StreamEvent::ThinkingEnd { sig: None })),
                        BlockKind::ToolUse { id } => {
                            out.push(Ok(StreamEvent::ToolCallEnd { id: id.clone(), sig: None }));
                        }
                    }
                }
            }

            "message_start" | "message_delta" | "ping" => {}
            "message_stop" => {
                out.push(Ok(StreamEvent::Done));
            }

            "error" => {
                let parsed: Value = serde_json::from_str(&sse.data).unwrap_or_default();
                let error_type = parsed["error"]["type"].as_str().unwrap_or("unknown");
                let error_msg = parsed["error"]["message"].as_str().unwrap_or("Unknown error").to_string();
                match error_type {
                    "rate_limit_error" => {
                        out.push(Err(ApiError::RateLimited { retry_after_ms: 5000 }));
                    }
                    _ => {
                        out.push(Err(ApiError::Api {
                            status: 0,
                            message: format!("{}: {}", error_type, error_msg),
                        }));
                    }
                }
            }

            other => { warn!("Unknown SSE event type: {}", other); }
        }

        out
    }
}

// -- Event stream adapter --

struct AnthropicStream {
    inner: Pin<Box<dyn Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Send>>,
    parser: SseParser,
    state: StreamState,
    pending: VecDeque<Result<StreamEvent, ApiError>>,
    done: bool,
    original_tools: Vec<ToolSchema>,
    is_oauth: bool,
}

impl Stream for AnthropicStream {
    type Item = Result<StreamEvent, ApiError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();

        loop {
            if let Some(event) = this.pending.pop_front() {
                return Poll::Ready(Some(event));
            }

            if this.done {
                return Poll::Ready(None);
            }

            match Pin::new(&mut this.inner).poll_next(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(None) => {
                    this.done = true;
                    for sse in this.parser.flush() {
                        let events = this.state.interpret(&sse);
                        this.pending.extend(this.remap_tools(events));
                    }
                    if let Some(event) = this.pending.pop_front() {
                        return Poll::Ready(Some(event));
                    }
                    return Poll::Ready(None);
                }
                Poll::Ready(Some(Err(e))) => {
                    this.done = true;
                    return Poll::Ready(Some(Err(ApiError::Http(e))));
                }
                Poll::Ready(Some(Ok(chunk))) => {
                    let text = String::from_utf8_lossy(&chunk);
                    for sse in this.parser.feed(&text) {
                        let events = this.state.interpret(&sse);
                        this.pending.extend(this.remap_tools(events));
                    }
                }
            }
        }
    }
}

impl AnthropicStream {
    fn remap_tools(&self, events: Vec<Result<StreamEvent, ApiError>>) -> Vec<Result<StreamEvent, ApiError>> {
        if !self.is_oauth {
            return events;
        }
        events.into_iter().map(|e| {
            e.map(|evt| match evt {
                StreamEvent::ToolCallStart { id, name } => {
                    let original = from_claude_code_name(&name, &self.original_tools);
                    StreamEvent::ToolCallStart { id, name: original }
                }
                other => other,
            })
        }).collect()
    }
}

pub fn event_stream(
    bytes: Pin<Box<dyn Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Send>>,
    tools: &[ToolSchema],
    is_oauth: bool,
) -> EventStream {
    Box::pin(AnthropicStream {
        inner: bytes,
        parser: SseParser::new(),
        state: StreamState::new(),
        pending: VecDeque::new(),
        done: false,
        original_tools: tools.to_vec(),
        is_oauth,
    })
}
