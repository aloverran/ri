// Print mode: single-shot prompt -> stream output -> exit.
//
// Subscribes to agent events and writes text/tool output to stdout.
// Two sub-modes:
//   text (default): only final text content to stdout, metadata to stderr
//   json: all events as JSON lines to stdout

use ri_core::event::{AgentEvent, AssistantStreamEvent, EventReceiver};
use std::io::Write;

pub async fn run_text(mut rx: EventReceiver) {
    while let Ok(event) = rx.recv().await {
        match event {
            AgentEvent::MessageUpdate(ref stream_event) => match stream_event {
                AssistantStreamEvent::TextDelta { delta } => {
                    let stdout = std::io::stdout();
                    let mut out = stdout.lock();
                    let _ = out.write_all(delta.as_bytes());
                    let _ = out.flush();
                }
                AssistantStreamEvent::ToolCallStart { name, .. } => {
                    eprintln!("\n[tool: {name}]");
                }
                AssistantStreamEvent::ToolCallEnd { .. } => {
                    eprintln!();
                }
                AssistantStreamEvent::Error { message } => {
                    eprintln!("Error: {message}");
                }
                _ => {}
            },
            AgentEvent::ToolExecutionEnd {
                result, is_error, ..
            } => {
                for block in &result {
                    if let ri_core::types::ContentBlock::Text { text } = block {
                        if is_error {
                            eprintln!("[tool error] {text}");
                        }
                    }
                }
            }
            AgentEvent::AgentEnd => break,
            _ => {}
        }
    }

    // Final newline
    println!();
}

pub async fn run_json(mut rx: EventReceiver) {
    while let Ok(event) = rx.recv().await {
        let json = event_to_json(&event);
        {
            let stdout = std::io::stdout();
            let mut out = stdout.lock();
            let _ = out.write_all(json.as_bytes());
            let _ = out.write_all(b"\n");
            let _ = out.flush();
        }

        if matches!(event, AgentEvent::AgentEnd) {
            break;
        }
    }
}

pub fn event_to_json_value(event: &AgentEvent) -> serde_json::Value {
    use serde_json::json;

    match event {
        AgentEvent::AgentStart => json!({"type": "agent_start"}),
        AgentEvent::AgentEnd => json!({"type": "agent_end"}),
        AgentEvent::TurnStart => json!({"type": "turn_start"}),
        AgentEvent::TurnEnd => json!({"type": "turn_end"}),
        AgentEvent::MessageStart => json!({"type": "message_start"}),
        AgentEvent::MessageEnd => json!({"type": "message_end"}),
        AgentEvent::MessageUpdate(stream_event) => {
            match stream_event {
                AssistantStreamEvent::TextStart => {
                    json!({"type": "message_update", "event": "text_start"})
                }
                AssistantStreamEvent::TextDelta { delta } => {
                    json!({"type": "message_update", "event": "text_delta", "delta": delta})
                }
                AssistantStreamEvent::TextEnd => {
                    json!({"type": "message_update", "event": "text_end"})
                }
                AssistantStreamEvent::ThinkingStart => {
                    json!({"type": "message_update", "event": "thinking_start"})
                }
                AssistantStreamEvent::ThinkingDelta { delta } => {
                    json!({"type": "message_update", "event": "thinking_delta", "delta": delta})
                }
                AssistantStreamEvent::ThinkingEnd => {
                    json!({"type": "message_update", "event": "thinking_end"})
                }
                AssistantStreamEvent::ToolCallStart { id, name } => {
                    json!({"type": "message_update", "event": "toolcall_start", "id": id, "name": name})
                }
                AssistantStreamEvent::ToolCallDelta { id, delta } => {
                    json!({"type": "message_update", "event": "toolcall_delta", "id": id, "delta": delta})
                }
                AssistantStreamEvent::ToolCallEnd { id } => {
                    json!({"type": "message_update", "event": "toolcall_end", "id": id})
                }
                AssistantStreamEvent::Done => {
                    json!({"type": "message_update", "event": "done"})
                }
                AssistantStreamEvent::Error { message } => {
                    json!({"type": "message_update", "event": "error", "message": message})
                }
            }
        }
        AgentEvent::ToolExecutionStart { tool_call } => json!({
            "type": "tool_execution_start",
            "toolCallId": tool_call.id,
            "toolName": tool_call.name,
            "args": tool_call.input
        }),
        AgentEvent::ToolExecutionUpdate {
            tool_call_id,
            partial,
        } => json!({
            "type": "tool_execution_update",
            "toolCallId": tool_call_id,
            "partial": partial.iter().map(content_block_to_json).collect::<Vec<_>>()
        }),
        AgentEvent::ToolExecutionEnd {
            tool_call_id,
            result,
            is_error,
        } => json!({
            "type": "tool_execution_end",
            "toolCallId": tool_call_id,
            "result": result.iter().map(content_block_to_json).collect::<Vec<_>>(),
            "isError": is_error
        }),
    }
}

fn event_to_json(event: &AgentEvent) -> String {
    event_to_json_value(event).to_string()
}

fn content_block_to_json(block: &ri_core::types::ContentBlock) -> serde_json::Value {
    use serde_json::json;
    match block {
        ri_core::types::ContentBlock::Text { text } => json!({"type": "text", "text": text}),
        ri_core::types::ContentBlock::Image { media_type, data } => {
            json!({"type": "image", "mediaType": media_type, "data": data})
        }
        ri_core::types::ContentBlock::ToolUse { id, name, input } => {
            json!({"type": "tool_use", "id": id, "name": name, "input": input})
        }
        ri_core::types::ContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
        } => json!({
            "type": "tool_result",
            "toolUseId": tool_use_id,
            "content": content.iter().map(content_block_to_json).collect::<Vec<_>>(),
            "isError": is_error
        }),
        ri_core::types::ContentBlock::Thinking { thinking } => {
            json!({"type": "thinking", "thinking": thinking})
        }
    }
}
