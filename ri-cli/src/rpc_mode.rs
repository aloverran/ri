use ri_agent::{self as agent, AgentCallback, AgentEvent, RunConfig};
use ri::{
    ContentBlock, LlmProvider, Message, Model, Role, SessionStore, ThinkingLevel, ToolDef,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::io::Write;
use std::path::PathBuf;
use tokio::io::AsyncBufReadExt;

use crate::print_mode;

#[derive(Debug, Deserialize)]
struct RpcCommand {
    #[serde(rename = "type")]
    command_type: String,
    #[serde(flatten)]
    data: Value,
}

struct RpcCallback;

impl AgentCallback for RpcCallback {
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
    thinking: ThinkingLevel,
) {
    let (mut filing, mut session_ids) = match SessionStore::init("rpc", &cwd.display().to_string(), &system_prompt) {
        Ok(v) => v,
        Err(e) => {
            output_json(&json!({"type": "error", "message": format!("Failed to init session: {}", e)}));
            return;
        }
    };
    let mut cb = RpcCallback;

    let config = RunConfig {
        model,
        system_prompt,
        tools,
        thinking,
        max_tokens: None,
        cwd,
        strategy: agent::naive_strategy(),
    };

    if let Some(prompt) = initial_prompt {
        let user_id = filing.next_id();
        let user_msg = Message::new(user_id.clone(), Role::User, vec![ContentBlock::text(&prompt)]);
        if let Err(e) = filing.write_message(user_msg) {
            output_json(&json!({"type": "error", "message": format!("Failed to write message: {}", e)}));
            return;
        }
        session_ids.push(user_id);

        let cancel = tokio_util::sync::CancellationToken::new();
        let _ = agent::run(provider.as_ref(), &config, &mut filing, &mut session_ids, &mut cb, cancel).await;
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

                let user_id = filing.next_id();
                let user_msg = Message::new(user_id.clone(), Role::User, vec![ContentBlock::text(message)]);
                if let Err(e) = filing.write_message(user_msg) {
                    output_json(&json!({"type": "error", "message": format!("Failed to write message: {}", e)}));
                    continue;
                }
                session_ids.push(user_id);

                let cancel = tokio_util::sync::CancellationToken::new();
                let _ = agent::run(provider.as_ref(), &config, &mut filing, &mut session_ids, &mut cb, cancel).await;

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
