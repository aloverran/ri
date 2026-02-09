use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Content {
    Text {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sig: Option<String>,
    },
    Thinking {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sig: Option<String>,
    },
    Image {
        media_type: String,
        data: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sig: Option<String>,
    },
    ToolResult {
        tool_use_id: String,
        output: String,
        #[serde(default)]
        is_error: bool,
    },
}

impl Content {
    pub fn text(s: impl Into<String>) -> Self {
        Content::Text { text: s.into(), sig: None }
    }

    pub fn thinking(s: impl Into<String>) -> Self {
        Content::Thinking { text: s.into(), sig: None }
    }

    pub fn tool_use(id: impl Into<String>, name: impl Into<String>, input: serde_json::Value) -> Self {
        Content::ToolUse { id: id.into(), name: name.into(), input, sig: None }
    }

    pub fn tool_result(id: impl Into<String>, output: impl Into<String>, is_error: bool) -> Self {
        Content::ToolResult { tool_use_id: id.into(), output: output.into(), is_error }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<Content>,
}

impl Message {
    pub fn user(text: impl Into<String>) -> Self {
        Message { role: Role::User, content: vec![Content::text(text)] }
    }

    pub fn assistant(text: impl Into<String>) -> Self {
        Message { role: Role::Assistant, content: vec![Content::text(text)] }
    }

    pub fn system(text: impl Into<String>) -> Self {
        Message { role: Role::System, content: vec![Content::text(text)] }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Model {
    pub id: String,
    pub name: String,
    pub reasoning: bool,
    pub context_window: usize,
    pub max_tokens: usize,
    pub cost: ModelCost,
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
pub enum ThinkingLevel {
    Off,
    Low,
    Medium,
    High,
    #[serde(rename = "xhigh")]
    XHigh,
}

#[derive(Debug, Clone)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub input: serde_json::Value,
}
