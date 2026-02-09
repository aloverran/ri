// Core message and model types for ri.
//
// These correspond to pi's Message, Model, and related types.
// We use closed enums rather than trait objects -- extensibility
// is handled via a Custom variant carrying serde_json::Value.

use serde::{Deserialize, Serialize};

// -- Roles --

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
}

// -- Content blocks --

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    Image {
        #[serde(rename = "mediaType")]
        media_type: String,
        data: String, // base64
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        #[serde(rename = "toolUseId")]
        tool_use_id: String,
        content: Vec<ContentBlock>,
        #[serde(default)]
        is_error: bool,
    },
    Thinking {
        thinking: String,
    },
}

// -- Messages --

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentBlock>,
}

// -- Agent messages (extensible via Custom variant) --

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentMessage {
    Llm(Message),
    Custom {
        custom_type: String,
        data: serde_json::Value,
    },
}

impl AgentMessage {
    pub fn user(text: impl Into<String>) -> Self {
        AgentMessage::Llm(Message {
            role: Role::User,
            content: vec![ContentBlock::Text { text: text.into() }],
        })
    }

    pub fn assistant(text: impl Into<String>) -> Self {
        AgentMessage::Llm(Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text { text: text.into() }],
        })
    }

    pub fn system(text: impl Into<String>) -> Self {
        AgentMessage::Llm(Message {
            role: Role::System,
            content: vec![ContentBlock::Text { text: text.into() }],
        })
    }
}

// -- Model definition --

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ApiType {
    #[serde(rename = "anthropic-messages")]
    AnthropicMessages,
    #[serde(rename = "openai-completions")]
    OpenaiCompletions,
    #[serde(rename = "openai-responses")]
    OpenaiResponses,
    #[serde(rename = "google")]
    Google,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelCost {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InputModality {
    Text,
    Image,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Model {
    pub id: String,
    pub name: String,
    pub api: ApiType,
    pub provider: String,
    pub base_url: String,
    pub reasoning: bool,
    pub input: Vec<InputModality>,
    pub cost: ModelCost,
    pub context_window: usize,
    pub max_tokens: usize,
}

// -- Thinking level --

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingLevel {
    Off,
    Low,
    Medium,
    High,
    #[serde(rename = "xhigh")]
    XHigh,
}

// -- Tool call and result types --

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub input: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    pub tool_use_id: String,
    pub content: Vec<ContentBlock>,
    pub is_error: bool,
}

// -- Completion options --

#[derive(Debug, Clone, Default)]
pub struct CompletionOptions {
    pub system_prompt: Option<String>,
    pub max_tokens: Option<usize>,
    pub thinking_level: Option<ThinkingLevel>,
    pub tools: Vec<ToolSchema>,
    pub stop_sequences: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSchema {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value, // JSON Schema
}
