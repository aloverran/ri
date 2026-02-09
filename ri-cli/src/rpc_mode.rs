// RPC mode: JSON-RPC over stdin/stdout.
//
// Commands arrive as JSON lines on stdin.
// Events and responses are written as JSON lines to stdout.
// Compatible with pi's RPC protocol.

use ri_core::event::EventReceiver;
use ri_core::types::AgentMessage;
use serde::Deserialize;
use serde_json::{json, Value};
use std::io::Write;
use tokio::io::AsyncBufReadExt;
use tokio::sync::mpsc;

#[derive(Debug, Deserialize)]
struct RpcCommand {
    id: Option<String>,
    #[serde(rename = "type")]
    command_type: String,
    #[serde(flatten)]
    data: Value,
}

fn output_json(value: &Value) {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let _ = serde_json::to_writer(&mut out, value);
    let _ = out.write_all(b"\n");
    let _ = out.flush();
}

fn success_response(id: Option<String>, command: &str, data: Option<Value>) -> Value {
    let mut resp = json!({
        "type": "response",
        "command": command,
        "success": true
    });
    if let Some(id) = id {
        resp["id"] = json!(id);
    }
    if let Some(data) = data {
        resp["data"] = data;
    }
    resp
}

fn error_response(id: Option<String>, command: &str, message: &str) -> Value {
    let mut resp = json!({
        "type": "response",
        "command": command,
        "success": false,
        "error": message
    });
    if let Some(id) = id {
        resp["id"] = json!(id);
    }
    resp
}

/// Run RPC mode. Spawns tasks for:
/// 1. Reading commands from stdin
/// 2. Forwarding agent events to stdout
/// 3. Processing commands
pub async fn run(
    mut event_rx: EventReceiver,
    steering_tx: mpsc::Sender<AgentMessage>,
    follow_up_tx: mpsc::Sender<AgentMessage>,
    cancel: tokio_util::sync::CancellationToken,
) {
    // Spawn event forwarder -- writes all agent events as JSON lines to stdout
    let event_cancel = cancel.clone();
    let event_task = tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = event_cancel.cancelled() => break,
                result = event_rx.recv() => {
                    match result {
                        Ok(event) => {
                            let json = super::print_mode::event_to_json_value(&event);
                            output_json(&json);
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            let msg = json!({"type": "warning", "message": format!("Dropped {n} events")});
                            output_json(&msg);
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
            }
        }
    });

    // Read commands from stdin
    let stdin = tokio::io::stdin();
    let reader = tokio::io::BufReader::new(stdin);
    let mut lines = reader.lines();

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            line = lines.next_line() => {
                match line {
                    Ok(Some(line_text)) => {
                        let text: String = line_text.trim().to_string();
                        if text.is_empty() {
                            continue;
                        }

                        match serde_json::from_str::<RpcCommand>(&text) {
                            Ok(cmd) => {
                                let response = handle_command(
                                    cmd,
                                    &steering_tx,
                                    &follow_up_tx,
                                    &cancel,
                                ).await;
                                output_json(&response);
                            }
                            Err(e) => {
                                output_json(&error_response(None, "parse", &format!("Invalid JSON: {e}")));
                            }
                        }
                    }
                    Ok(None) => break, // stdin closed
                    Err(e) => {
                        output_json(&error_response(None, "io", &format!("stdin read error: {e}")));
                        break;
                    }
                }
            }
        }
    }

    cancel.cancel();
    let _ = event_task.await;
}

async fn handle_command(
    cmd: RpcCommand,
    steering_tx: &mpsc::Sender<AgentMessage>,
    follow_up_tx: &mpsc::Sender<AgentMessage>,
    cancel: &tokio_util::sync::CancellationToken,
) -> Value {
    let id = cmd.id.clone();

    match cmd.command_type.as_str() {
        "prompt" => {
            let message = cmd.data.get("message").and_then(|v| v.as_str()).unwrap_or("");
            if message.is_empty() {
                return error_response(id, "prompt", "Missing 'message' field");
            }
            let msg = AgentMessage::user(message.to_string());
            match follow_up_tx.send(msg).await {
                Ok(_) => success_response(id, "prompt", None),
                Err(e) => error_response(id, "prompt", &format!("Failed to queue: {e}")),
            }
        }

        "steer" => {
            let message = cmd.data.get("message").and_then(|v| v.as_str()).unwrap_or("");
            if message.is_empty() {
                return error_response(id, "steer", "Missing 'message' field");
            }
            let msg = AgentMessage::user(message.to_string());
            match steering_tx.send(msg).await {
                Ok(_) => success_response(id, "steer", None),
                Err(e) => error_response(id, "steer", &format!("Failed to queue: {e}")),
            }
        }

        "follow_up" => {
            let message = cmd.data.get("message").and_then(|v| v.as_str()).unwrap_or("");
            if message.is_empty() {
                return error_response(id, "follow_up", "Missing 'message' field");
            }
            let msg = AgentMessage::user(message.to_string());
            match follow_up_tx.send(msg).await {
                Ok(_) => success_response(id, "follow_up", None),
                Err(e) => error_response(id, "follow_up", &format!("Failed to queue: {e}")),
            }
        }

        "abort" => {
            cancel.cancel();
            success_response(id, "abort", None)
        }

        "get_state" => {
            success_response(
                id,
                "get_state",
                Some(json!({
                    "isStreaming": !cancel.is_cancelled()
                })),
            )
        }

        other => error_response(id, other, &format!("Unknown command: {other}")),
    }
}
