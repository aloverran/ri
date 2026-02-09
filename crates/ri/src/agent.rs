// Agent loop -- a function, not a struct.
//
// The caller owns the message history. The loop runs one prompt
// to completion (possibly multiple tool-call rounds), emitting events
// via callback, and returns when done.
//
// Messages get IDs via a caller-provided generator so the agent loop
// doesn't depend on filing. Provenance is populated on assistant messages.

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
    // Emitted after each message is fully constructed (with ID and provenance).
    // The caller can use this to persist to filing.
    MessageComplete(Message),
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

// Callback trait for the agent loop. Combines ID generation and event handling
// into one object so the caller can use a single mutable borrow.
pub trait AgentCallback {
    fn next_id(&mut self) -> String;
    fn on_event(&mut self, event: AgentEvent);
}

// Run the agent loop: stream LLM response, execute tool calls, repeat.
//
// Messages include provenance (which input messages the LLM saw).
pub async fn run(
    config: &RunConfig<'_>,
    messages: &mut Vec<Message>,
    cb: &mut dyn AgentCallback,
    cancel: tokio_util::sync::CancellationToken,
) -> eyre::Result<()> {
    let tool_schemas: Vec<ToolSchema> = config.tools.iter().map(|t| t.schema()).collect();
    let tool_map: HashMap<&str, &ToolDef> = config.tools.iter()
        .map(|t| (t.name, t))
        .collect();

    loop {
        if cancel.is_cancelled() { break; }

        cb.on_event(AgentEvent::TurnStart);

        // Snapshot input message IDs for provenance (what the LLM will see).
        let input_ids: Vec<String> = messages.iter()
            .filter(|m| !m.id.is_empty())
            .map(|m| m.id.clone())
            .collect();

        let opts = RequestOptions {
            model: config.model,
            system_prompt: config.system_prompt,
            messages: messages,
            tools: &tool_schemas,
            thinking: config.thinking,
            max_tokens: config.max_tokens,
        };

        // Stream the LLM response.
        let mut stream = match config.provider.stream(&opts).await {
            Ok(s) => s,
            Err(e) => {
                cb.on_event(AgentEvent::Error(e.to_string()));
                cb.on_event(AgentEvent::TurnEnd);
                return Err(eyre::eyre!("{}", e));
            }
        };

        // Accumulate the response.
        let mut text_buf = String::new();
        let mut thinking_buf = String::new();
        let mut tool_calls: HashMap<String, (String, String)> = HashMap::new();
        let mut content_blocks: Vec<ContentBlock> = Vec::new();

        while let Some(event) = stream.next().await {
            if cancel.is_cancelled() { break; }

            match event {
                Ok(ref evt) => {
                    cb.on_event(AgentEvent::StreamEvent(evt.clone()));

                    match evt {
                        StreamEvent::TextStart => { text_buf.clear(); }
                        StreamEvent::TextDelta(d) => { text_buf.push_str(d); }
                        StreamEvent::TextEnd { sig } => {
                            if !text_buf.is_empty() {
                                let mut extra = HashMap::new();
                                if let Some(s) = sig {
                                    extra.insert("sig".to_string(), serde_json::Value::String(s.clone()));
                                }
                                content_blocks.push(ContentBlock::Text {
                                    text: std::mem::take(&mut text_buf),
                                    extra,
                                });
                            }
                        }
                        StreamEvent::ThinkingStart => { thinking_buf.clear(); }
                        StreamEvent::ThinkingDelta(d) => { thinking_buf.push_str(d); }
                        StreamEvent::ThinkingEnd { sig } => {
                            if !thinking_buf.is_empty() {
                                let mut extra = HashMap::new();
                                if let Some(s) = sig {
                                    extra.insert("sig".to_string(), serde_json::Value::String(s.clone()));
                                }
                                content_blocks.push(ContentBlock::Thinking {
                                    thinking: std::mem::take(&mut thinking_buf),
                                    extra,
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
                                let mut extra = HashMap::new();
                                if let Some(s) = sig {
                                    extra.insert("sig".to_string(), serde_json::Value::String(s.clone()));
                                }
                                content_blocks.push(ContentBlock::ToolUse {
                                    id: id.clone(),
                                    name,
                                    input,
                                    extra,
                                });
                            }
                        }
                        StreamEvent::Done => {}
                        StreamEvent::Error(msg) => {
                            cb.on_event(AgentEvent::Error(msg.clone()));
                        }
                    }
                }
                Err(e) => {
                    cb.on_event(AgentEvent::Error(e.to_string()));
                    cb.on_event(AgentEvent::TurnEnd);
                    return Err(eyre::eyre!("{}", e));
                }
            }
        }

        // Flush incomplete buffers.
        if !text_buf.is_empty() {
            content_blocks.push(ContentBlock::text(text_buf));
        }
        if !thinking_buf.is_empty() {
            content_blocks.push(ContentBlock::thinking(thinking_buf));
        }
        for (id, (name, json)) in tool_calls {
            let input = serde_json::from_str(&json)
                .unwrap_or_else(|_| serde_json::json!({ "error": "Interrupted", "partial": json }));
            content_blocks.push(ContentBlock::tool_use(id, name, input));
        }

        // Build assistant message with ID and provenance.
        let ts = chrono::Utc::now().to_rfc3339();
        let assistant_msg = Message {
            id: cb.next_id(),
            role: Role::Assistant,
            content: content_blocks.clone(),
            provenance: Some(Provenance {
                input: input_ids,
                model: config.model.id.clone(),
                ts,
                usage: None, // TODO: capture from stream when providers emit it
            }),
            meta: None,
            extra: HashMap::new(),
        };
        cb.on_event(AgentEvent::MessageComplete(assistant_msg.clone()));
        messages.push(assistant_msg);

        // Extract and execute tool calls.
        let calls: Vec<ToolCall> = content_blocks.iter().filter_map(|c| match c {
            ContentBlock::ToolUse { id, name, input, .. } => Some(ToolCall {
                id: id.clone(), name: name.clone(), input: input.clone(),
            }),
            _ => None,
        }).collect();

        if calls.is_empty() {
            cb.on_event(AgentEvent::TurnEnd);
            break;
        }

        // Execute tool calls.
        let mut results: Vec<ContentBlock> = Vec::new();
        for call in &calls {
            if cancel.is_cancelled() {
                results.push(ContentBlock::tool_result_text(&call.id, "Cancelled", true));
                continue;
            }

            cb.on_event(AgentEvent::ToolStart { id: call.id.clone(), name: call.name.clone() });

            let output = match tool_map.get(call.name.as_str()) {
                Some(tool) => {
                    (tool.run)(call.input.clone(), config.cwd.to_path_buf(), cancel.clone()).await
                }
                None => ToolOutput {
                    text: format!("Tool '{}' not found", call.name),
                    is_error: true,
                },
            };

            cb.on_event(AgentEvent::ToolEnd {
                id: call.id.clone(),
                output: output.text.clone(),
                is_error: output.is_error,
            });

            results.push(ContentBlock::tool_result_text(&call.id, &output.text, output.is_error));
        }

        // Append tool results as a user message (with ID, no provenance -- authored by system).
        let tool_msg = Message::new(
            cb.next_id(),
            Role::User,
            results,
        );
        cb.on_event(AgentEvent::MessageComplete(tool_msg.clone()));
        messages.push(tool_msg);

        cb.on_event(AgentEvent::TurnEnd);
    }

    Ok(())
}
