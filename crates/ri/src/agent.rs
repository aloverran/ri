// Agent loop -- a function, not a struct.
//
// The caller owns the message history. The loop runs one prompt
// to completion (possibly multiple tool-call rounds), emitting events
// via callback, and returns when done.
//
// To steer:    cancel, modify messages, call run() again.
// To follow up: wait for run() to return, add a message, call run() again.

use std::collections::HashMap;
use std::path::Path;
use futures::StreamExt;

use crate::types::*;
use crate::api::{Provider, RequestOptions, StreamEvent, ToolSchema};
use crate::tools::{ToolDef, ToolOutput};

// What the agent loop emits to the caller.
#[derive(Debug, Clone)]
pub enum AgentEvent {
    TurnStart,
    TurnEnd,
    StreamEvent(StreamEvent),
    ToolStart { id: String, name: String },
    ToolEnd { id: String, output: String, is_error: bool },
    Error(String),
}

// Everything the agent loop needs for one run.
pub struct RunConfig<'a> {
    pub provider: &'a Provider,
    pub model: &'a Model,
    pub system_prompt: &'a str,
    pub tools: &'a [ToolDef],
    pub thinking: ThinkingLevel,
    pub max_tokens: Option<usize>,
    pub cwd: &'a Path,
}

// Run the agent loop: stream LLM response, execute tool calls, repeat.
//
// Returns when the LLM produces no tool calls (natural stop) or
// the cancellation token fires.
pub async fn run(
    config: &RunConfig<'_>,
    messages: &mut Vec<Message>,
    on_event: &mut dyn FnMut(AgentEvent),
    cancel: tokio_util::sync::CancellationToken,
) -> eyre::Result<()> {
    let tool_schemas: Vec<ToolSchema> = config.tools.iter().map(|t| t.schema()).collect();
    let tool_map: HashMap<&str, &ToolDef> = config.tools.iter()
        .map(|t| (t.name, t))
        .collect();

    loop {
        if cancel.is_cancelled() { break; }

        on_event(AgentEvent::TurnStart);

        // Build request options from current state
        let opts = RequestOptions {
            model: config.model,
            system_prompt: config.system_prompt,
            messages: messages,
            tools: &tool_schemas,
            thinking: config.thinking,
            max_tokens: config.max_tokens,
        };

        // Stream the LLM response
        let mut stream = match config.provider.stream(&opts).await {
            Ok(s) => s,
            Err(e) => {
                on_event(AgentEvent::Error(e.to_string()));
                on_event(AgentEvent::TurnEnd);
                return Err(eyre::eyre!("{}", e));
            }
        };

        // Accumulate the response
        let mut text_buf = String::new();
        let mut thinking_buf = String::new();
        let mut tool_calls: HashMap<String, (String, String)> = HashMap::new(); // id -> (name, json)
        let mut content_blocks: Vec<Content> = Vec::new();

        // Track current signatures
        let mut text_sig: Option<String> = None;
        let mut thinking_sig: Option<String> = None;

        while let Some(event) = stream.next().await {
            if cancel.is_cancelled() { break; }

            match event {
                Ok(ref evt) => {
                    on_event(AgentEvent::StreamEvent(evt.clone()));

                    match evt {
                        StreamEvent::TextStart => { text_buf.clear(); text_sig = None; }
                        StreamEvent::TextDelta(d) => { text_buf.push_str(d); }
                        StreamEvent::TextEnd { sig } => {
                            if !text_buf.is_empty() {
                                content_blocks.push(Content::Text {
                                    text: std::mem::take(&mut text_buf),
                                    sig: sig.clone().or(text_sig.take()),
                                });
                            }
                        }
                        StreamEvent::ThinkingStart => { thinking_buf.clear(); thinking_sig = None; }
                        StreamEvent::ThinkingDelta(d) => { thinking_buf.push_str(d); }
                        StreamEvent::ThinkingEnd { sig } => {
                            if !thinking_buf.is_empty() {
                                content_blocks.push(Content::Thinking {
                                    text: std::mem::take(&mut thinking_buf),
                                    sig: sig.clone().or(thinking_sig.take()),
                                });
                            }
                        }
                        StreamEvent::ToolCallStart { id, name } => {
                            tool_calls.insert(id.clone(), (name.clone(), String::new()));
                        }
                        StreamEvent::ToolCallDelta { id, json_fragment } => {
                            if let Some((_, json)) = tool_calls.get_mut(id) {
                                json.push_str(json_fragment);
                            }
                        }
                        StreamEvent::ToolCallEnd { id, sig } => {
                            if let Some((name, json)) = tool_calls.remove(id) {
                                let input: serde_json::Value = serde_json::from_str(&json)
                                    .unwrap_or_else(|_| serde_json::json!({
                                        "error": "Invalid JSON from model",
                                        "partial": json,
                                    }));
                                content_blocks.push(Content::ToolUse {
                                    id: id.clone(),
                                    name,
                                    input,
                                    sig: sig.clone(),
                                });
                            }
                        }
                        StreamEvent::Done => {}
                        StreamEvent::Error(msg) => {
                            on_event(AgentEvent::Error(msg.clone()));
                        }
                    }
                }
                Err(e) => {
                    on_event(AgentEvent::Error(e.to_string()));
                    on_event(AgentEvent::TurnEnd);
                    return Err(eyre::eyre!("{}", e));
                }
            }
        }

        // Flush any incomplete buffers (handles stream interruption)
        if !text_buf.is_empty() {
            content_blocks.push(Content::Text { text: text_buf, sig: None });
        }
        if !thinking_buf.is_empty() {
            content_blocks.push(Content::Thinking { text: thinking_buf, sig: None });
        }
        for (id, (name, json)) in tool_calls {
            let input = serde_json::from_str(&json)
                .unwrap_or_else(|_| serde_json::json!({ "error": "Interrupted", "partial": json }));
            content_blocks.push(Content::ToolUse { id, name, input, sig: None });
        }

        // Append assistant message
        messages.push(Message {
            role: Role::Assistant,
            content: content_blocks.clone(),
        });

        // Extract and execute tool calls
        let calls: Vec<ToolCall> = content_blocks.iter().filter_map(|c| match c {
            Content::ToolUse { id, name, input, .. } => Some(ToolCall {
                id: id.clone(), name: name.clone(), input: input.clone(),
            }),
            _ => None,
        }).collect();

        if calls.is_empty() {
            on_event(AgentEvent::TurnEnd);
            break; // No tool calls = natural stop
        }

        // Execute tool calls
        let mut results: Vec<Content> = Vec::new();
        for call in &calls {
            if cancel.is_cancelled() {
                results.push(Content::tool_result(&call.id, "Cancelled", true));
                continue;
            }

            on_event(AgentEvent::ToolStart { id: call.id.clone(), name: call.name.clone() });

            let output = match tool_map.get(call.name.as_str()) {
                Some(tool) => {
                    (tool.run)(call.input.clone(), config.cwd.to_path_buf(), cancel.clone()).await
                }
                None => ToolOutput {
                    text: format!("Tool '{}' not found", call.name),
                    is_error: true,
                },
            };

            on_event(AgentEvent::ToolEnd {
                id: call.id.clone(),
                output: output.text.clone(),
                is_error: output.is_error,
            });

            results.push(Content::tool_result(&call.id, &output.text, output.is_error));
        }

        // Append tool results as a user message
        messages.push(Message { role: Role::User, content: results });

        on_event(AgentEvent::TurnEnd);
        // Loop back for next LLM turn
    }

    Ok(())
}
