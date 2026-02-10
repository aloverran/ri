use ri_agent::{AgentCallback, AgentEvent};
use ri_core::event::StreamEvent;
use std::io::Write;

pub struct TextCallback;

impl AgentCallback for TextCallback {
    fn on_event(&mut self, evt: AgentEvent) {
        on_event_text(&evt);
    }
}

pub struct JsonCallback;

impl AgentCallback for JsonCallback {
    fn on_event(&mut self, evt: AgentEvent) {
        on_event_json(&evt);
    }
}

pub fn on_event_text(evt: &AgentEvent) {
    match evt {
        AgentEvent::StreamEvent(se) => match se {
            StreamEvent::TextDelta(d) => {
                let stdout = std::io::stdout();
                let mut out = stdout.lock();
                let _ = out.write_all(d.as_bytes());
                let _ = out.flush();
            }
            StreamEvent::ToolCallStart { name, .. } => {
                eprintln!("\n[tool: {}]", name);
            }
            StreamEvent::Error(msg) => {
                eprintln!("Error: {}", msg);
            }
            _ => {}
        },
        AgentEvent::ToolEnd { output, is_error, .. } => {
            if *is_error {
                eprintln!("[tool error] {}", output);
            }
        }
        _ => {}
    }
}

pub fn on_event_json(evt: &AgentEvent) {
    let json = event_to_json(evt);
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let _ = serde_json::to_writer(&mut out, &json);
    let _ = out.write_all(b"\n");
    let _ = out.flush();
}

pub fn event_to_json(evt: &AgentEvent) -> serde_json::Value {
    use serde_json::json;
    match evt {
        AgentEvent::TurnStart => json!({"type": "turn_start"}),
        AgentEvent::TurnEnd => json!({"type": "turn_end"}),
        AgentEvent::Error(msg) => json!({"type": "error", "message": msg}),
        AgentEvent::StreamEvent(se) => match se {
            StreamEvent::TextStart => json!({"type": "text_start"}),
            StreamEvent::TextDelta(d) => json!({"type": "text_delta", "delta": d}),
            StreamEvent::TextEnd { .. } => json!({"type": "text_end"}),
            StreamEvent::ThinkingStart => json!({"type": "thinking_start"}),
            StreamEvent::ThinkingDelta(d) => json!({"type": "thinking_delta", "delta": d}),
            StreamEvent::ThinkingEnd { .. } => json!({"type": "thinking_end"}),
            StreamEvent::ToolCallStart { id, name } => json!({"type": "toolcall_start", "id": id, "name": name}),
            StreamEvent::ToolCallDelta { id, json_fragment } => json!({"type": "toolcall_delta", "id": id, "delta": json_fragment}),
            StreamEvent::ToolCallEnd { id, .. } => json!({"type": "toolcall_end", "id": id}),
            StreamEvent::Done => json!({"type": "done"}),
            StreamEvent::Error(msg) => json!({"type": "stream_error", "message": msg}),
        },
        AgentEvent::ToolStart { id, name } => json!({"type": "tool_start", "id": id, "name": name}),
        AgentEvent::ToolEnd { id, output, is_error } => json!({
            "type": "tool_end", "id": id, "output": output, "is_error": is_error
        }),
        AgentEvent::MessageComplete(msg) => json!({
            "type": "message_complete", "id": &msg.id, "role": msg.role
        }),
    }
}
