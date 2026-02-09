// Agent loop -- the core execution loop.
//
// Mirrors pi's agent-loop.ts:
//   Outer loop: follow-up messages
//     Inner loop: tool execution + steering
//       - Process pending messages
//       - Stream LLM response
//       - Execute tool calls (check steering after each)

use std::collections::HashMap;
use std::sync::Arc;

use futures::StreamExt;
use tokio::sync::mpsc;
use tracing::warn;

use crate::event::{AgentEvent, AssistantStreamEvent, EventSender};
use crate::provider::{LlmProvider, ProviderError};
use crate::tool::Tool;
use crate::types::{
    AgentMessage, CompletionOptions, ContentBlock, Message, Model, Role, ThinkingLevel, ToolCall,
};

pub struct AgentConfig {
    pub model: Model,
    pub thinking_level: ThinkingLevel,
    pub system_prompt: String,
    pub api_key: String,
}

pub struct Agent {
    pub config: AgentConfig,
    pub messages: Vec<AgentMessage>,
    pub provider: Arc<dyn LlmProvider>,
    pub tools: HashMap<String, Arc<dyn Tool>>,
    pub event_tx: EventSender,
    steering_tx: mpsc::Sender<AgentMessage>,
    steering_rx: mpsc::Receiver<AgentMessage>,
    follow_up_tx: mpsc::Sender<AgentMessage>,
    follow_up_rx: mpsc::Receiver<AgentMessage>,
    cancel: tokio_util::sync::CancellationToken,
}

impl Agent {
    pub fn new(
        config: AgentConfig,
        provider: Arc<dyn LlmProvider>,
        tools: Vec<Arc<dyn Tool>>,
        event_tx: EventSender,
    ) -> Self {
        let tool_map: HashMap<String, Arc<dyn Tool>> = tools
            .into_iter()
            .map(|t| (t.name().to_string(), t))
            .collect();

        let (steering_tx, steering_rx) = mpsc::channel(64);
        let (follow_up_tx, follow_up_rx) = mpsc::channel(64);

        Self {
            config,
            messages: Vec::new(),
            provider,
            tools: tool_map,
            event_tx,
            steering_tx,
            steering_rx,
            follow_up_tx,
            follow_up_rx,
            cancel: tokio_util::sync::CancellationToken::new(),
        }
    }

    /// Get a handle for sending steering messages from outside the loop.
    pub fn steering_handle(&self) -> mpsc::Sender<AgentMessage> {
        self.steering_tx.clone()
    }

    /// Get a handle for sending follow-up messages from outside the loop.
    pub fn follow_up_handle(&self) -> mpsc::Sender<AgentMessage> {
        self.follow_up_tx.clone()
    }

    pub fn abort(&self) {
        self.cancel.cancel();
    }

    pub fn cancel_token(&self) -> tokio_util::sync::CancellationToken {
        self.cancel.clone()
    }

    /// Run the agent loop for a prompt.
    /// This is the main entry point -- prompt -> stream -> tool calls -> loop.
    pub async fn prompt(&mut self, input: AgentMessage) -> eyre::Result<()> {
        self.messages.push(input);
        self.run_loop().await
    }

    /// Drain all pending messages from an mpsc channel without blocking.
    fn drain_channel(rx: &mut mpsc::Receiver<AgentMessage>) -> Vec<AgentMessage> {
        let mut out = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            out.push(msg);
        }
        out
    }

    async fn run_loop(&mut self) -> eyre::Result<()> {
        let _ = self.event_tx.send(AgentEvent::AgentStart);

        // Check for steering messages at start (user may have typed while waiting)
        let mut pending = Self::drain_channel(&mut self.steering_rx);

        // Outer loop: continues when queued follow-up messages arrive after agent would stop
        loop {
            if self.cancel.is_cancelled() {
                break;
            }

            let mut has_more_tool_calls = true;
            let mut steering_after_tools: Option<Vec<AgentMessage>> = None;

            // Inner loop: process tool calls and steering messages
            while has_more_tool_calls || !pending.is_empty() {
                if self.cancel.is_cancelled() {
                    break;
                }

                let _ = self.event_tx.send(AgentEvent::TurnStart);

                // Process pending messages (inject before next assistant response)
                if !pending.is_empty() {
                    for msg in pending.drain(..) {
                        self.messages.push(msg);
                    }
                }

                // Stream assistant response
                let assistant_msg =
                    match self.stream_assistant_response().await {
                        Ok(msg) => msg,
                        Err(e) => {
                            let _ = self.event_tx.send(AgentEvent::TurnEnd);
                            let _ = self.event_tx.send(AgentEvent::AgentEnd);
                            return Err(e);
                        }
                    };

                // Add assistant message to history
                self.messages.push(AgentMessage::Llm(Message {
                    role: Role::Assistant,
                    content: assistant_msg.content.clone(),
                }));

                // Extract tool calls
                let tool_calls: Vec<ToolCall> = assistant_msg
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::ToolUse { id, name, input, .. } => Some(ToolCall {
                            id: id.clone(),
                            name: name.clone(),
                            input: input.clone(),
                        }),
                        _ => None,
                    })
                    .collect();

                has_more_tool_calls = !tool_calls.is_empty();

                if has_more_tool_calls {
                    let (results, steering) =
                        self.execute_tool_calls(&tool_calls).await;

                    // Add tool results to message history
                    for result in &results {
                        self.messages.push(AgentMessage::Llm(Message {
                            role: Role::User,
                            content: vec![ContentBlock::ToolResult {
                                tool_use_id: result.id.clone(),
                                content: result.content.clone(),
                                is_error: result.is_error,
                            }],
                        }));
                    }

                    steering_after_tools = steering;
                }

                let _ = self.event_tx.send(AgentEvent::TurnEnd);

                // Get steering messages after turn completes
                if let Some(msgs) = steering_after_tools.take() {
                    if !msgs.is_empty() {
                        pending = msgs;
                    }
                } else {
                    pending = Self::drain_channel(&mut self.steering_rx);
                }
            }

            // Agent would stop here. Check for follow-up messages.
            let follow_ups = Self::drain_channel(&mut self.follow_up_rx);
            if !follow_ups.is_empty() {
                pending = follow_ups;
                continue;
            }

            break;
        }

        let _ = self.event_tx.send(AgentEvent::AgentEnd);
        Ok(())
    }

    /// Stream an assistant response from the LLM.
    /// Converts AgentMessages to LLM-compatible Messages, calls the provider,
    /// and collects the streamed response while forwarding events.
    async fn stream_assistant_response(&self) -> eyre::Result<AssistantResponse> {
        // Convert AgentMessages to LLM-compatible Messages
        let llm_messages: Vec<Message> = self
            .messages
            .iter()
            .filter_map(|m| match m {
                AgentMessage::Llm(msg) if msg.role != Role::System => Some(msg.clone()),
                _ => None,
            })
            .collect();

        // Build completion options
        let tool_schemas: Vec<_> = self.tools.values().map(|t| t.schema()).collect();
        let options = CompletionOptions {
            system_prompt: Some(self.config.system_prompt.clone()),
            max_tokens: None,
            thinking_level: Some(self.config.thinking_level),
            tools: tool_schemas,
            stop_sequences: Vec::new(),
        };

        let _ = self.event_tx.send(AgentEvent::MessageStart);

        // Call provider to get stream
        let mut stream = self
            .provider
            .stream(&self.config.model, &llm_messages, &options, &self.config.api_key)
            .await
            .map_err(|e| match e {
                ProviderError::RateLimited { retry_after_ms } => {
                    eyre::eyre!("Rate limited, retry after {retry_after_ms}ms")
                }
                ProviderError::ContextOverflow { used, limit } => {
                    eyre::eyre!("Context overflow: {used}/{limit} tokens")
                }
                other => eyre::eyre!("{other}"),
            })?;

        // Consume the stream, collecting the response.
        // Uses a HashMap for tool calls to handle potential interleaving.
        let mut content_blocks: Vec<ContentBlock> = Vec::new();
        let mut current_text = String::new();
        let mut current_thinking = String::new();
        let mut pending_tools: HashMap<String, (String, String)> = HashMap::new();

        while let Some(event) = stream.next().await {
            if self.cancel.is_cancelled() {
                break;
            }

            match event {
                Ok(ref evt) => {
                    let _ = self
                        .event_tx
                        .send(AgentEvent::MessageUpdate(evt.clone()));

                    match evt {
                        AssistantStreamEvent::TextStart => {
                            current_text.clear();
                        }
                        AssistantStreamEvent::TextDelta { delta } => {
                            current_text.push_str(delta);
                        }
                        AssistantStreamEvent::TextEnd { signature } => {
                            if !current_text.is_empty() {
                                content_blocks.push(ContentBlock::Text {
                                    text: std::mem::take(&mut current_text),
                                    text_signature: signature.clone(),
                                });
                            }
                        }
                        AssistantStreamEvent::ThinkingStart => {
                            current_thinking.clear();
                        }
                        AssistantStreamEvent::ThinkingDelta { delta } => {
                            current_thinking.push_str(delta);
                        }
                        AssistantStreamEvent::ThinkingEnd { signature } => {
                            if !current_thinking.is_empty() {
                                content_blocks.push(ContentBlock::Thinking {
                                    thinking: std::mem::take(&mut current_thinking),
                                    thinking_signature: signature.clone(),
                                });
                            }
                        }
                        AssistantStreamEvent::ToolCallStart { id, name } => {
                            pending_tools
                                .insert(id.clone(), (name.clone(), String::new()));
                        }
                        AssistantStreamEvent::ToolCallDelta { id, delta } => {
                            if let Some((_, json)) = pending_tools.get_mut(id) {
                                json.push_str(delta);
                            }
                        }
                        AssistantStreamEvent::ToolCallEnd { id, signature } => {
                            if let Some((name, json)) = pending_tools.remove(id) {
                                let input: serde_json::Value =
                                    serde_json::from_str(&json).unwrap_or_else(|_| {
                                        serde_json::json!({
                                            "error": "Invalid JSON from model",
                                            "partial": json
                                        })
                                    });
                                content_blocks.push(ContentBlock::ToolUse {
                                    id: id.clone(),
                                    name,
                                    input,
                                    thought_signature: signature.clone(),
                                });
                            }
                        }
                        AssistantStreamEvent::Done => {}
                        AssistantStreamEvent::Error { message } => {
                            warn!("Stream error: {message}");
                        }
                    }
                }
                Err(e) => {
                    let _ = self.event_tx.send(AgentEvent::MessageUpdate(
                        AssistantStreamEvent::Error {
                            message: e.to_string(),
                        },
                    ));
                    let _ = self.event_tx.send(AgentEvent::MessageEnd);
                    return Err(eyre::eyre!("Stream error: {e}"));
                }
            }
        }

        // Flush any content that was accumulating when the stream ended
        // (handles interruption without proper *End events)
        if !current_text.is_empty() {
            content_blocks.push(ContentBlock::Text {
                text: current_text,
                text_signature: None,
            });
        }
        if !current_thinking.is_empty() {
            content_blocks.push(ContentBlock::Thinking {
                thinking: current_thinking,
                thinking_signature: None,
            });
        }
        for (id, (name, json)) in pending_tools {
            let input: serde_json::Value =
                serde_json::from_str(&json).unwrap_or_else(|_| {
                    serde_json::json!({
                        "error": "Interrupted/Invalid JSON",
                        "partial": json
                    })
                });
            content_blocks.push(ContentBlock::ToolUse { id, name, input, thought_signature: None });
        }

        let _ = self.event_tx.send(AgentEvent::MessageEnd);

        Ok(AssistantResponse {
            content: content_blocks,
        })
    }

    /// Execute tool calls, checking for steering messages after each.
    /// Returns (tool results, optional steering messages that interrupted execution).
    async fn execute_tool_calls(
        &mut self,
        tool_calls: &[ToolCall],
    ) -> (Vec<ToolResultEntry>, Option<Vec<AgentMessage>>) {
        let mut results = Vec::new();

        for (i, call) in tool_calls.iter().enumerate() {
            if self.cancel.is_cancelled() {
                // Skip remaining tool calls
                for skipped in &tool_calls[i..] {
                    results.push(self.skip_tool_call(skipped));
                }
                break;
            }

            let _ = self.event_tx.send(AgentEvent::ToolExecutionStart {
                tool_call: call.clone(),
            });

            let result = match self.tools.get(&call.name) {
                Some(tool) => {
                    let (update_tx, mut update_rx) = mpsc::channel(16);
                    let tool = tool.clone();
                    let call_id = call.id.clone();
                    let params = call.input.clone();
                    let cancel = self.cancel.clone();
                    let event_tx_clone = self.event_tx.clone();
                    let call_id_for_updates = call.id.clone();

                    // Spawn tool execution
                    let handle = tokio::spawn(async move {
                        tool.execute(&call_id, params, cancel, Some(update_tx))
                            .await
                    });

                    // Forward updates while tool runs. The update channel closes
                    // when the tool drops its sender (on completion or error),
                    // so recv() returns None and we break out. Then we await the handle.
                    while let Some(u) = update_rx.recv().await {
                        let _ = event_tx_clone.send(AgentEvent::ToolExecutionUpdate {
                            tool_call_id: call_id_for_updates.clone(),
                            partial: u.content,
                        });
                    }

                    match handle.await {
                        Ok(Ok(output)) => ToolResultEntry {
                            id: call.id.clone(),
                            content: output.content,
                            is_error: output.is_error,
                        },
                        Ok(Err(e)) => ToolResultEntry {
                            id: call.id.clone(),
                            content: vec![ContentBlock::text(format!("{e}"))],
                            is_error: true,
                        },
                        Err(e) => ToolResultEntry {
                            id: call.id.clone(),
                            content: vec![ContentBlock::text(format!("Tool panicked: {e}"))],
                            is_error: true,
                        },
                    }
                }
                None => ToolResultEntry {
                    id: call.id.clone(),
                    content: vec![ContentBlock::text(format!("Tool '{}' not found", call.name))],
                    is_error: true,
                },
            };

            let _ = self.event_tx.send(AgentEvent::ToolExecutionEnd {
                tool_call_id: call.id.clone(),
                result: result.content.clone(),
                is_error: result.is_error,
            });

            results.push(result);

            // Check for steering messages after each tool execution
            let steering = Self::drain_channel(&mut self.steering_rx);
            if !steering.is_empty() {
                // Skip remaining tool calls
                for skipped in &tool_calls[i + 1..] {
                    results.push(self.skip_tool_call(skipped));
                }
                return (results, Some(steering));
            }
        }

        (results, None)
    }

    fn skip_tool_call(&self, call: &ToolCall) -> ToolResultEntry {
        let _ = self.event_tx.send(AgentEvent::ToolExecutionStart {
            tool_call: call.clone(),
        });
        let result = ToolResultEntry {
            id: call.id.clone(),
            content: vec![ContentBlock::text("Skipped due to queued user message.")],
            is_error: true,
        };
        let _ = self.event_tx.send(AgentEvent::ToolExecutionEnd {
            tool_call_id: call.id.clone(),
            result: result.content.clone(),
            is_error: true,
        });
        result
    }
}

struct AssistantResponse {
    content: Vec<ContentBlock>,
}

struct ToolResultEntry {
    id: String,
    content: Vec<ContentBlock>,
    is_error: bool,
}
