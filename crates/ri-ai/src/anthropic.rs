// Anthropic Messages API provider.
//
// Implements streaming via SSE (Server-Sent Events) with manual line parsing.
// Reference: https://docs.anthropic.com/en/api/messages-streaming

use async_trait::async_trait;
use futures::Stream;
use reqwest::header::{HeaderMap, HeaderValue, CONTENT_TYPE};
use ri_core::event::AssistantStreamEvent;
use ri_core::provider::{LlmProvider, ProviderError, StreamOutput};
use ri_core::types::{
    CompletionOptions, ContentBlock, Message, Model, Role, ThinkingLevel, ToolSchema,
};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::pin::Pin;
use std::task::{Context, Poll};
use tracing::warn;

pub struct AnthropicProvider {
    client: reqwest::Client,
}

impl AnthropicProvider {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }
}

// -- Request body construction --

fn build_system_param(system_prompt: &str) -> Value {
    json!([{
        "type": "text",
        "text": system_prompt,
        "cache_control": { "type": "ephemeral" }
    }])
}

fn convert_content_block(block: &ContentBlock) -> Value {
    match block {
        ContentBlock::Text { text } => json!({
            "type": "text",
            "text": text
        }),
        ContentBlock::Image { media_type, data } => json!({
            "type": "image",
            "source": {
                "type": "base64",
                "media_type": media_type,
                "data": data
            }
        }),
        ContentBlock::ToolUse { id, name, input } => json!({
            "type": "tool_use",
            "id": id,
            "name": name,
            "input": input
        }),
        ContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
        } => {
            let content_blocks: Vec<Value> = content.iter().map(convert_content_block).collect();
            json!({
                "type": "tool_result",
                "tool_use_id": tool_use_id,
                "content": content_blocks,
                "is_error": is_error
            })
        }
        ContentBlock::Thinking { thinking } => json!({
            "type": "thinking",
            "thinking": thinking,
            // Thinking blocks in history need a signature; but for now
            // we pass them as-is and let the API handle validation.
        }),
    }
}

fn convert_message(msg: &Message) -> Value {
    let role = match msg.role {
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::System => {
            warn!("System message in messages array -- converting to user role");
            "user"
        }
    };

    // Tool results must be sent as user messages in the Anthropic API
    let content: Vec<Value> = msg.content.iter().map(convert_content_block).collect();

    // Special case: if the only blocks are ToolResult, role must be "user"
    let has_tool_results = msg
        .content
        .iter()
        .any(|b| matches!(b, ContentBlock::ToolResult { .. }));
    let effective_role = if has_tool_results { "user" } else { role };

    json!({
        "role": effective_role,
        "content": content
    })
}

fn convert_tool(tool: &ToolSchema) -> Value {
    json!({
        "name": tool.name,
        "description": tool.description,
        "input_schema": {
            "type": "object",
            "properties": tool.parameters.get("properties").unwrap_or(&json!({})),
            "required": tool.parameters.get("required").unwrap_or(&json!([]))
        }
    })
}

fn build_request_body(model: &Model, messages: &[Message], options: &CompletionOptions) -> Value {
    let api_messages: Vec<Value> = messages
        .iter()
        .filter(|m| m.role != Role::System)
        .map(convert_message)
        .collect();

    let max_tokens = options
        .max_tokens
        .unwrap_or_else(|| (model.max_tokens / 3).max(4096));

    let mut body = json!({
        "model": model.id,
        "messages": api_messages,
        "max_tokens": max_tokens,
        "stream": true
    });

    // System prompt as top-level field
    if let Some(ref sys) = options.system_prompt {
        body["system"] = build_system_param(sys);
    }

    // Tools
    if !options.tools.is_empty() {
        let tools: Vec<Value> = options.tools.iter().map(convert_tool).collect();
        body["tools"] = json!(tools);
    }

    // Stop sequences
    if !options.stop_sequences.is_empty() {
        body["stop_sequences"] = json!(options.stop_sequences);
    }

    // Thinking / reasoning
    if model.reasoning {
        if let Some(level) = options.thinking_level {
            match level {
                ThinkingLevel::Off => {}
                _ => {
                    let is_opus_46 =
                        model.id.contains("opus-4-6") || model.id.contains("opus-4.6");
                    if is_opus_46 {
                        // Adaptive thinking for Opus 4.6+
                        body["thinking"] = json!({ "type": "adaptive" });
                        let effort = match level {
                            ThinkingLevel::Low => "low",
                            ThinkingLevel::Medium => "medium",
                            ThinkingLevel::High => "high",
                            ThinkingLevel::XHigh => "max",
                            ThinkingLevel::Off => unreachable!(),
                        };
                        body["output_config"] = json!({ "effort": effort });
                    } else {
                        // Budget-based thinking for older models
                        let budget = match level {
                            ThinkingLevel::Low => 1024,
                            ThinkingLevel::Medium => 4096,
                            ThinkingLevel::High => 16384,
                            ThinkingLevel::XHigh => 32768,
                            ThinkingLevel::Off => unreachable!(),
                        };
                        body["thinking"] = json!({
                            "type": "enabled",
                            "budget_tokens": budget
                        });
                    }
                }
            }
        }
    }

    body
}

// -- SSE parsing --

/// Tracks the state of content blocks during streaming.
#[derive(Debug)]
enum BlockKind {
    Text,
    Thinking,
    ToolUse { id: String },
}

/// Parses a single SSE data payload (JSON) and yields AssistantStreamEvents.
fn parse_sse_event(
    event_type: &str,
    data: &str,
    blocks: &mut Vec<BlockKind>,
) -> Vec<Result<AssistantStreamEvent, ProviderError>> {
    let mut events = Vec::new();

    match event_type {
        "message_start" => {
            // Contains usage info; we don't track usage at the stream level currently.
        }

        "content_block_start" => {
            let parsed: Value = match serde_json::from_str(data) {
                Ok(v) => v,
                Err(e) => {
                    events.push(Err(ProviderError::StreamParse(format!(
                        "content_block_start: {e}"
                    ))));
                    return events;
                }
            };

            let block_type = parsed["content_block"]["type"].as_str().unwrap_or("");
            let index = parsed["index"].as_u64().unwrap_or(0) as usize;

            match block_type {
                "text" => {
                    // Ensure blocks vec is big enough
                    while blocks.len() <= index {
                        blocks.push(BlockKind::Text);
                    }
                    blocks[index] = BlockKind::Text;
                    events.push(Ok(AssistantStreamEvent::TextStart));
                }
                "thinking" => {
                    while blocks.len() <= index {
                        blocks.push(BlockKind::Text);
                    }
                    blocks[index] = BlockKind::Thinking;
                    events.push(Ok(AssistantStreamEvent::ThinkingStart));
                }
                "tool_use" => {
                    let id = parsed["content_block"]["id"]
                        .as_str()
                        .unwrap_or("")
                        .to_string();
                    let name = parsed["content_block"]["name"]
                        .as_str()
                        .unwrap_or("")
                        .to_string();
                    while blocks.len() <= index {
                        blocks.push(BlockKind::Text);
                    }
                    blocks[index] = BlockKind::ToolUse { id: id.clone() };
                    events.push(Ok(AssistantStreamEvent::ToolCallStart { id, name }));
                }
                other => {
                    warn!("Unknown content block type: {other}");
                }
            }
        }

        "content_block_delta" => {
            let parsed: Value = match serde_json::from_str(data) {
                Ok(v) => v,
                Err(e) => {
                    events.push(Err(ProviderError::StreamParse(format!(
                        "content_block_delta: {e}"
                    ))));
                    return events;
                }
            };

            let index = parsed["index"].as_u64().unwrap_or(0) as usize;
            let delta_type = parsed["delta"]["type"].as_str().unwrap_or("");

            match delta_type {
                "text_delta" => {
                    let text = parsed["delta"]["text"].as_str().unwrap_or("").to_string();
                    events.push(Ok(AssistantStreamEvent::TextDelta { delta: text }));
                }
                "thinking_delta" => {
                    let thinking = parsed["delta"]["thinking"]
                        .as_str()
                        .unwrap_or("")
                        .to_string();
                    events.push(Ok(AssistantStreamEvent::ThinkingDelta { delta: thinking }));
                }
                "input_json_delta" => {
                    let partial = parsed["delta"]["partial_json"]
                        .as_str()
                        .unwrap_or("")
                        .to_string();
                    if let Some(block) = blocks.get(index) {
                        if let BlockKind::ToolUse { id, .. } = block {
                            events.push(Ok(AssistantStreamEvent::ToolCallDelta {
                                id: id.clone(),
                                delta: partial,
                            }));
                        }
                    }
                }
                "signature_delta" => {
                    // Thinking block signature; we don't expose this in our event model.
                }
                other => {
                    warn!("Unknown delta type: {other}");
                }
            }
        }

        "content_block_stop" => {
            let parsed: Value = match serde_json::from_str(data) {
                Ok(v) => v,
                Err(e) => {
                    events.push(Err(ProviderError::StreamParse(format!(
                        "content_block_stop: {e}"
                    ))));
                    return events;
                }
            };

            let index = parsed["index"].as_u64().unwrap_or(0) as usize;
            if let Some(block) = blocks.get(index) {
                match block {
                    BlockKind::Text => events.push(Ok(AssistantStreamEvent::TextEnd)),
                    BlockKind::Thinking => events.push(Ok(AssistantStreamEvent::ThinkingEnd)),
                    BlockKind::ToolUse { id, .. } => {
                        events.push(Ok(AssistantStreamEvent::ToolCallEnd { id: id.clone() }));
                    }
                }
            }
        }

        "message_delta" => {
            // Contains stop_reason and final usage. We emit Done on message_stop.
        }

        "message_stop" => {
            events.push(Ok(AssistantStreamEvent::Done));
        }

        "ping" => {
            // Keepalive, ignore.
        }

        "error" => {
            let parsed: Value = serde_json::from_str(data).unwrap_or_default();
            let error_type = parsed["error"]["type"].as_str().unwrap_or("unknown");
            let error_message = parsed["error"]["message"]
                .as_str()
                .unwrap_or("Unknown error")
                .to_string();

            match error_type {
                "overloaded_error" => {
                    events.push(Err(ProviderError::Other(
                        format!("API overloaded: {error_message}"),
                    )));
                }
                "rate_limit_error" => {
                    events.push(Err(ProviderError::RateLimited {
                        retry_after_ms: 5000,
                    }));
                }
                _ => {
                    events.push(Err(ProviderError::Api {
                        status: 0,
                        message: format!("{error_type}: {error_message}"),
                    }));
                }
            }
        }

        other => {
            warn!("Unknown SSE event type: {other}");
        }
    }

    events
}

// -- SSE byte stream adapter --

/// Wraps a reqwest byte stream and yields parsed AssistantStreamEvents.
struct SseStream {
    inner: Pin<Box<dyn Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Send>>,
    buffer: String,
    blocks: Vec<BlockKind>,
    pending: VecDeque<Result<AssistantStreamEvent, ProviderError>>,
    done: bool,
}

impl Stream for SseStream {
    type Item = Result<AssistantStreamEvent, ProviderError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();

        loop {
            // Drain any pending events first
            if !this.pending.is_empty() {
                return Poll::Ready(Some(this.pending.pop_front().unwrap()));
            }

            if this.done {
                return Poll::Ready(None);
            }

            // Poll the inner byte stream
            match Pin::new(&mut this.inner).poll_next(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(None) => {
                    this.done = true;
                    // Process any remaining data in buffer
                    if !this.buffer.is_empty() {
                        this.process_buffer();
                        if !this.pending.is_empty() {
                            return Poll::Ready(Some(this.pending.pop_front().unwrap()));
                        }
                    }
                    return Poll::Ready(None);
                }
                Poll::Ready(Some(Err(e))) => {
                    this.done = true;
                    return Poll::Ready(Some(Err(ProviderError::Http(e))));
                }
                Poll::Ready(Some(Ok(chunk))) => {
                    let text = String::from_utf8_lossy(&chunk);
                    this.buffer.push_str(&text);
                    this.process_buffer();
                    // Loop back to drain pending
                }
            }
        }
    }
}

impl SseStream {
    fn new(inner: Pin<Box<dyn Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Send>>) -> Self {
        Self {
            inner,
            buffer: String::new(),
            blocks: Vec::new(),
            pending: VecDeque::new(),
            done: false,
        }
    }

    /// Process complete SSE events from the buffer.
    /// SSE format: "event: <type>\ndata: <json>\n\n"
    fn process_buffer(&mut self) {
        // Process all complete events (separated by double newline)
        while let Some(end) = self.buffer.find("\n\n") {
            let event_block = self.buffer[..end].to_string();
            self.buffer = self.buffer[end + 2..].to_string();

            let mut event_type = String::new();
            let mut data = String::new();

            for line in event_block.lines() {
                if let Some(rest) = line.strip_prefix("event: ") {
                    event_type = rest.trim().to_string();
                } else if let Some(rest) = line.strip_prefix("data: ") {
                    if !data.is_empty() {
                        data.push('\n');
                    }
                    data.push_str(rest);
                } else if line.starts_with(':') {
                    // SSE comment, ignore
                }
            }

            if !event_type.is_empty() {
                let events = parse_sse_event(&event_type, &data, &mut self.blocks);
                self.pending.extend(events);
            }
        }
    }
}

/// Parse an error response body from the Anthropic API.
fn parse_error_body(status: u16, body: &str) -> ProviderError {
    let parsed: Value = serde_json::from_str(body).unwrap_or_default();
    let error_type = parsed["error"]["type"].as_str().unwrap_or("unknown");
    let error_message = parsed["error"]["message"]
        .as_str()
        .unwrap_or("Unknown error")
        .to_string();

    match error_type {
        "overloaded_error" | "invalid_request_error"
            if error_message.contains("token") && error_message.contains("exceed") =>
        {
            ProviderError::ContextOverflow {
                used: 0,
                limit: 0,
            }
        }
        "rate_limit_error" => ProviderError::RateLimited {
            retry_after_ms: 5000,
        },
        _ => ProviderError::Api {
            status,
            message: format!("{error_type}: {error_message}"),
        },
    }
}

// -- Provider implementation --

#[async_trait]
impl LlmProvider for AnthropicProvider {
    fn name(&self) -> &str {
        "anthropic"
    }

    async fn stream(
        &self,
        model: &Model,
        messages: &[Message],
        options: &CompletionOptions,
        api_key: &str,
    ) -> Result<StreamOutput, ProviderError> {
        let body = build_request_body(model, messages, options);
        let url = format!("{}/v1/messages", model.base_url.trim_end_matches('/'));

        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", HeaderValue::from_str(api_key).map_err(|e| {
            ProviderError::Other(format!("Invalid API key header: {e}"))
        })?);
        headers.insert(
            "anthropic-version",
            HeaderValue::from_static("2023-06-01"),
        );
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert("accept", HeaderValue::from_static("text/event-stream"));

        // Beta features
        let beta_value = "interleaved-thinking-2025-05-14";
        headers.insert(
            "anthropic-beta",
            HeaderValue::from_str(beta_value)
                .unwrap_or_else(|_| HeaderValue::from_static("interleaved-thinking-2025-05-14")),
        );

        let response = self
            .client
            .post(&url)
            .headers(headers)
            .body(body.to_string())
            .send()
            .await?;

        let status = response.status().as_u16();
        if status >= 400 {
            let body_text = response.text().await.unwrap_or_default();
            return Err(parse_error_body(status, &body_text));
        }

        let byte_stream = response.bytes_stream();
        let sse = SseStream::new(Box::pin(byte_stream));

        Ok(Box::pin(sse))
    }
}
