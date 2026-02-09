use ri::agent::{self, AgentEvent, RunConfig};
use ri::api::Provider;
use ri::tools::ToolDef;
use ri::types::*;
use serde::Deserialize;
use serde_json::{json, Value};
use std::io::Write;
use std::path::PathBuf;
use tokio::io::AsyncBufReadExt;

use crate::print_mode;

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

pub async fn run(
    provider: Provider,
    model: Model,
    system_prompt: String,
    tools: Vec<ToolDef>,
    cwd: PathBuf,
    initial_prompt: Option<String>,
) {
    let mut messages: Vec<Message> = Vec::new();

    // If there's an initial prompt, start the agent
    if let Some(prompt) = initial_prompt {
        messages.push(Message::user(prompt));

        let cancel = tokio_util::sync::CancellationToken::new();
        let config = RunConfig {
            provider: &provider,
            model: &model,
            system_prompt: &system_prompt,
            tools: &tools,
            thinking: ThinkingLevel::Medium,
            max_tokens: None,
            cwd: &cwd,
        };

        let _ = agent::run(&config, &mut messages, &mut |evt| {
            output_json(&print_mode::event_to_json(&evt));
        }, cancel).await;
    }

    // Read commands from stdin
    let stdin = tokio::io::stdin();
    let reader = tokio::io::BufReader::new(stdin);
    let mut lines = reader.lines();

    while let Ok(Some(line)) = lines.next_line().await {
        let text = line.trim().to_string();
        if text.is_empty() { continue; }

        let cmd: RpcCommand = match serde_json::from_str(&text) {
            Ok(c) => c,
            Err(e) => {
                output_json(&json!({"type": "error", "message": format!("Invalid JSON: {}", e)}));
                continue;
            }
        };

        match cmd.command_type.as_str() {
            "prompt" | "follow_up" => {
                let message = cmd.data.get("message").and_then(|v| v.as_str()).unwrap_or("");
                if message.is_empty() {
                    output_json(&json!({"type": "error", "message": "Missing 'message'"}));
                    continue;
                }

                messages.push(Message::user(message));

                let cancel = tokio_util::sync::CancellationToken::new();
                let config = RunConfig {
                    provider: &provider,
                    model: &model,
                    system_prompt: &system_prompt,
                    tools: &tools,
                    thinking: ThinkingLevel::Medium,
                    max_tokens: None,
                    cwd: &cwd,
                };

                let _ = agent::run(&config, &mut messages, &mut |evt| {
                    output_json(&print_mode::event_to_json(&evt));
                }, cancel).await;

                output_json(&json!({"type": "response", "command": "prompt", "success": true}));
            }

            "abort" => {
                output_json(&json!({"type": "response", "command": "abort", "success": true}));
                break;
            }

            other => {
                output_json(&json!({"type": "error", "message": format!("Unknown command: {}", other)}));
            }
        }
    }
}
