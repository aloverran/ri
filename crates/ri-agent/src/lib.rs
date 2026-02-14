//! Agent loop -- MessagePool-aware.
//!
//! The loop operates on the MessagePool (via SessionStore) rather than a bare Vec.
//! A ContextStrategy selects which messages from the pool to send each turn.
//! New messages (assistant responses, tool results) are written through filing
//! (which puts them in the pool AND appends to the active session file).

use std::collections::HashMap;
use std::path::PathBuf;
use futures::StreamExt;

use ri::{
    ContentBlock, JsonMap, LlmProvider, Message, MessagePool, Model, Provenance, RequestOptions,
    Role, SessionStore, StreamEvent, ThinkingLevel, ToolDef, ToolOutput, ToolSchema, Usage,
};

/// In-progress tool call being accumulated from streaming deltas.
struct PendingToolCall {
    name: String,
    json_buf: String,
}

/// Events emitted by the agent loop for display, logging, or RPC output.
#[derive(Debug, Clone)]
pub enum AgentEvent {
    TurnStart,
    TurnEnd,
    StreamEvent(StreamEvent),
    ToolStart { id: String, name: String },
    ToolEnd { id: String, output: String, is_error: bool },
    Error(String),
    /// Emitted after each message is fully constructed and persisted.
    MessageComplete(Message),
}

/// Given the pool and current session message IDs, return the ordered
/// list of message IDs to include in the next LLM call.
pub type ContextStrategy = Box<dyn Fn(&MessagePool, &[String]) -> Vec<String> + Send + Sync>;

/// Naive strategy: include all session message IDs in order (no compaction).
pub fn naive_strategy() -> ContextStrategy {
    Box::new(|_pool, session_ids| session_ids.to_vec())
}

/// Everything the agent loop needs for one run.
pub struct RunConfig {
    pub model: Model,
    pub system_prompt: String,
    pub tools: Vec<ToolDef>,
    pub thinking: ThinkingLevel,
    pub max_tokens: Option<usize>,
    pub cwd: PathBuf,
    pub strategy: ContextStrategy,
}

/// Callback trait for event observation (display, logging, RPC output).
pub trait AgentCallback {
    fn on_event(&mut self, event: AgentEvent);
}

/// Run the agent loop: compose context from pool, stream LLM response,
/// execute tool calls, persist everything, repeat until the model stops
/// issuing tool calls.
pub async fn run(
    provider: &dyn LlmProvider,
    config: &RunConfig,
    filing: &mut SessionStore,
    session_ids: &mut Vec<String>,
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

        // Strategy selects message IDs for this LLM call.
        let input_ids = (config.strategy)(&filing.pool, session_ids);

        // Resolve messages from the pool.
        let messages: Vec<Message> = filing.pool.resolve_existing(&input_ids)
            .into_iter()
            .cloned()
            .collect();

        let opts = RequestOptions {
            model: config.model.clone(),
            system_prompt: config.system_prompt.clone(),
            messages,
            tools: tool_schemas.clone(),
            thinking: config.thinking,
            max_tokens: config.max_tokens,
        };

        // Stream the LLM response.
        let mut stream = match provider.stream(opts).await {
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
        let mut tool_calls: HashMap<String, PendingToolCall> = HashMap::new();
        let mut content_blocks: Vec<ContentBlock> = Vec::new();
        let mut usage: Option<Usage> = None;

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
                                let mut extra = JsonMap::new();
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
                                let mut extra = JsonMap::new();
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
                            tool_calls.insert(id.clone(), PendingToolCall {
                                name: name.clone(),
                                json_buf: String::new(),
                            });
                        }
                        StreamEvent::ToolCallDelta { id, json_fragment } => {
                            if let Some(tc) = tool_calls.get_mut(id) {
                                tc.json_buf.push_str(json_fragment);
                            }
                        }
                        StreamEvent::ToolCallEnd { id, sig } => {
                            if let Some(tc) = tool_calls.remove(id) {
                                let input: serde_json::Value = serde_json::from_str(&tc.json_buf)
                                    .unwrap_or_else(|_| serde_json::json!({
                                        "error": "Invalid JSON from model",
                                        "partial": tc.json_buf,
                                    }));
                                let mut extra = JsonMap::new();
                                if let Some(s) = sig {
                                    extra.insert("sig".to_string(), serde_json::Value::String(s.clone()));
                                }
                                content_blocks.push(ContentBlock::ToolUse {
                                    id: id.clone(),
                                    name: tc.name,
                                    input,
                                    extra,
                                });
                            }
                        }
                        StreamEvent::Usage(u) => { usage = Some(u.clone()); }
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
        for (id, tc) in tool_calls {
            let input = serde_json::from_str(&tc.json_buf)
                .unwrap_or_else(|_| serde_json::json!({ "error": "Interrupted", "partial": tc.json_buf }));
            content_blocks.push(ContentBlock::tool_use(id, tc.name, input));
        }

        // Build assistant message with provenance.
        let assistant_id = filing.next_id();
        let ts = chrono::Utc::now().to_rfc3339();
        let assistant_msg = Message {
            id: assistant_id.clone(),
            role: Role::Assistant,
            content: content_blocks.clone(),
            provenance: Some(Provenance {
                input: input_ids,
                model: config.model.id.clone(),
                ts,
                usage,
            }),
            meta: None,
            extra: JsonMap::new(),
        };
        filing.write_message(assistant_msg.clone())?;
        session_ids.push(assistant_id);
        cb.on_event(AgentEvent::MessageComplete(assistant_msg));

        // Extract tool calls from the response.
        let calls: Vec<(String, String, serde_json::Value)> = content_blocks.iter().filter_map(|c| match c {
            ContentBlock::ToolUse { id, name, input, .. } => Some((
                id.clone(), name.clone(), input.clone(),
            )),
            _ => None,
        }).collect();

        if calls.is_empty() {
            cb.on_event(AgentEvent::TurnEnd);
            break;
        }

        // Execute tool calls.
        let mut results: Vec<ContentBlock> = Vec::new();
        for (call_id, call_name, call_input) in &calls {
            if cancel.is_cancelled() {
                results.push(ContentBlock::tool_result_text(call_id, "Cancelled", true));
                continue;
            }

            cb.on_event(AgentEvent::ToolStart { id: call_id.clone(), name: call_name.clone() });

            let output = match tool_map.get(call_name.as_str()) {
                Some(tool) => {
                    (tool.run)(call_input.clone(), config.cwd.to_path_buf(), cancel.clone()).await
                }
                None => ToolOutput {
                    text: format!("Tool '{}' not found", call_name),
                    is_error: true,
                },
            };

            cb.on_event(AgentEvent::ToolEnd {
                id: call_id.clone(),
                output: output.text.clone(),
                is_error: output.is_error,
            });

            results.push(ContentBlock::tool_result_text(call_id, &output.text, output.is_error));
        }

        // Write tool results as a user message.
        let tool_id = filing.next_id();
        let tool_msg = Message::new(tool_id.clone(), Role::User, results);
        filing.write_message(tool_msg.clone())?;
        session_ids.push(tool_id);
        cb.on_event(AgentEvent::MessageComplete(tool_msg));

        cb.on_event(AgentEvent::TurnEnd);
    }

    Ok(())
}
