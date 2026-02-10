use ri_core::agent::{self, AgentCallback, AgentEvent, RunConfig};
use ri_core::provider::LlmProvider;
use ri_core::tool::ToolDef;
use ri_core::types::*;
use ri_store::types::Message;
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

struct RpcCallback { counter: u64 }

impl RpcCallback {
    fn new() -> Self { RpcCallback { counter: 0 } }
}

impl AgentCallback for RpcCallback {
    fn next_id(&mut self) -> String {
        self.counter += 1;
        format!("rpc_{}", self.counter)
    }

    fn on_event(&mut self, evt: AgentEvent) {
        output_json(&print_mode::event_to_json(&evt));
    }
}

fn output_json(value: &Value) {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let _ = serde_json::to_writer(&mut out, value);
    let _ = out.write_all(b"\n");
    let _ = out.flush();
}

pub async fn run(
    provider: Box<dyn LlmProvider>,
    model: Model,
    system_prompt: String,
    tools: Vec<ToolDef>,
    cwd: PathBuf,
    initial_prompt: Option<String>,
) {
    let mut messages: Vec<Message> = Vec::new();
    let mut cb = RpcCallback::new();

    if let Some(prompt) = initial_prompt {
        messages.push(Message::user(prompt));

        let cancel = tokio_util::sync::CancellationToken::new();
        let config = RunConfig {
            provider: provider.as_ref(),
            model: &model,
            system_prompt: &system_prompt,
            tools: &tools,
            thinking: ThinkingLevel::Medium,
            max_tokens: None,
            cwd: &cwd,
        };

        let _ = agent::run(&config, &mut messages, &mut cb, cancel).await;
    }

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
                    provider: provider.as_ref(),
                    model: &model,
                    system_prompt: &system_prompt,
                    tools: &tools,
                    thinking: ThinkingLevel::Medium,
                    max_tokens: None,
                    cwd: &cwd,
                };

                let _ = agent::run(&config, &mut messages, &mut cb, cancel).await;

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
