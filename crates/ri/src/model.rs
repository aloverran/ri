//! Core data model: messages and contexts.
//!
//! Two primitives:
//!
//! - **Message**: an immutable content blob (role + content blocks). The atom.
//! - **Context**: an immutable object -- an ordered list of message
//!   references, parent links, and metadata. Contexts form a DAG
//!   through their parents. A session is just a pointer to one.
//!
//! The LLM API is `f(context.messages) -> Message`. Everything else
//! is algebra on contexts: creating them, composing them, pointing
//! at them.

use std::borrow::Borrow;
use std::fmt;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

// String newtypes for message, context, and session identifiers.
// Separate types so the compiler catches mix-ups.

macro_rules! string_id {
    ($name:ident, $doc:expr) => {
        #[doc = $doc]
        #[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn new(s: impl Into<String>) -> Self { Self(s.into()) }
            pub fn as_str(&self) -> &str { &self.0 }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str(&self.0) }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str { &self.0 }
        }

        impl Borrow<str> for $name {
            fn borrow(&self) -> &str { &self.0 }
        }

        impl From<String> for $name {
            fn from(s: String) -> Self { Self(s) }
        }

        impl From<&str> for $name {
            fn from(s: &str) -> Self { Self(s.to_string()) }
        }
    };
}

string_id!(MessageId, "Unique identifier for a message in the pool.");
string_id!(ContextId, "Unique identifier for a context in the history DAG.");
string_id!(SessionId, "File-stem identifier for a session (e.g. \"2026-02-28_120000_fix-login\").");

// -- Message --

/// Immutable content blob. The atomic unit of the system.
///
/// Messages live in the pool and are referenced by ID from contexts.
/// They carry no provenance -- that belongs to the context that
/// introduced them.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub id: MessageId,
    pub role: Role,
    pub content: Vec<ContentBlock>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<serde_json::Value>,
}

impl Message {
    /// Short human-readable summary for git-log style session views.
    pub fn summarize(&self) -> String {
        let role_tag = match self.role {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
        };

        if self.content.is_empty() {
            return format!("[{}] (empty)", role_tag);
        }

        let fixed_len = role_tag.len() + 3; // "[" + role + "] "
        let content_budget = SUMMARY_WIDTH.saturating_sub(fixed_len);
        let content_summary = summarize_blocks(&self.content, content_budget);
        format!("[{}] {}", role_tag, content_summary)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
}

/// A typed piece of content within a message.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    Thinking {
        thinking: String,
        /// Provider signature for replaying thinking blocks (Anthropic, Gemini).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sig: Option<String>,
    },
    Image {
        #[serde(rename = "mediaType")]
        media_type: String,
        data: String,
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
        /// Structured data for UI rendering. Not sent to the LLM.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        details: Option<serde_json::Value>,
    },
    Error {
        message: String,
    },
    // Catch-all for unknown block types -- preserves round-trip.
    #[serde(untagged)]
    Unknown(serde_json::Value),
}

impl ContentBlock {
    pub fn text(s: impl Into<String>) -> Self {
        ContentBlock::Text { text: s.into() }
    }

    pub fn thinking(s: impl Into<String>) -> Self {
        ContentBlock::Thinking { thinking: s.into(), sig: None }
    }

    pub fn tool_use(id: impl Into<String>, name: impl Into<String>, input: serde_json::Value) -> Self {
        ContentBlock::ToolUse { id: id.into(), name: name.into(), input }
    }

    pub fn tool_result(tool_use_id: impl Into<String>, content: Vec<ContentBlock>, is_error: bool, details: Option<serde_json::Value>) -> Self {
        ContentBlock::ToolResult { tool_use_id: tool_use_id.into(), content, is_error, details }
    }

    pub fn tool_result_text(tool_use_id: impl Into<String>, text: impl Into<String>, is_error: bool, details: Option<serde_json::Value>) -> Self {
        ContentBlock::ToolResult {
            tool_use_id: tool_use_id.into(),
            content: vec![ContentBlock::text(text)],
            is_error,
            details,
        }
    }

    pub fn error(s: impl Into<String>) -> Self {
        ContentBlock::Error { message: s.into() }
    }

    /// Short human-readable summary of this block, targeting ~800 chars.
    pub fn summarize(&self) -> String {
        match self {
            ContentBlock::Text { text } => {
                truncate_with_ellipsis(text, SUMMARY_WIDTH)
            }
            ContentBlock::Thinking { thinking, .. } => {
                let tag = "[thinking] ";
                let body = truncate_with_ellipsis(thinking, SUMMARY_WIDTH - tag.len());
                format!("{tag}{body}")
            }
            ContentBlock::Image { media_type, .. } => {
                format!("[image: {media_type}]")
            }
            ContentBlock::ToolUse { name, input, .. } => {
                let tag = format!("[tool: {name}] ");
                let body = truncate_with_ellipsis(&input.to_string(), SUMMARY_WIDTH.saturating_sub(tag.len()));
                format!("{tag}{body}")
            }
            ContentBlock::ToolResult { is_error, content, .. } => {
                let tag = if *is_error { "[tool error] " } else { "[tool result] " };
                let inner: String = content.iter().map(|b| match b {
                    ContentBlock::Text { text } => text.as_str(),
                    _ => "",
                }).collect::<Vec<_>>().join(" ");
                let body = truncate_with_ellipsis(&inner, SUMMARY_WIDTH - tag.len());
                format!("{tag}{body}")
            }
            ContentBlock::Error { message } => {
                let tag = "[error] ";
                let body = truncate_with_ellipsis(message, SUMMARY_WIDTH - tag.len());
                format!("{tag}{body}")
            }
            ContentBlock::Unknown(v) => {
                truncate_with_ellipsis(&v.to_string(), SUMMARY_WIDTH)
            }
        }
    }
}

// -- Context --

/// An immutable object: an ordered list of message references, parent
/// links, and metadata. The fundamental unit of the system alongside
/// Message.
///
/// Resolved against the pool, `messages` gives you `Vec<Message>` --
/// what you hand to the LLM. Parent links form a DAG. A session is
/// just a pointer to a context.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Context {
    pub id: ContextId,
    pub messages: Vec<MessageId>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parents: Vec<ContextId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<serde_json::Value>,
}

// -- Supporting types --

/// Token usage from a single LLM call.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_read_tokens: u64,
    #[serde(default)]
    pub cache_write_tokens: u64,
    /// Raw provider-specific usage data for debug display.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extras: Option<serde_json::Value>,
}

/// Generate a globally unique ID (UUID v4, hex, no dashes).
pub fn gen_id() -> String {
    Uuid::new_v4().simple().to_string()
}

// Summarization helpers (private)

/// Target width for single-block summaries. Tagged blocks subtract their
/// prefix length from this budget so the total stays consistent.
const SUMMARY_WIDTH: usize = 150;

fn summarize_blocks(blocks: &[ContentBlock], budget: usize) -> String {
    if blocks.len() == 1 {
        return truncate_with_ellipsis(&blocks[0].summarize(), budget);
    }
    let separator_cost = (blocks.len() - 1) * 3;
    let per_block = budget.saturating_sub(separator_cost) / blocks.len();
    let parts: Vec<String> = blocks.iter().map(|b| {
        truncate_with_ellipsis(&b.summarize(), per_block)
    }).collect();
    parts.join(" | ")
}

fn truncate_with_ellipsis(s: &str, max_chars: usize) -> String {
    let s = s.trim();
    // Fast path: for ASCII strings, byte length == char count.
    if s.len() <= max_chars && s.is_ascii() {
        return s.to_string();
    }
    let char_count = s.chars().count();
    if char_count <= max_chars {
        return s.to_string();
    }
    let cut: String = s.chars().take(max_chars).collect();
    // Search the last 30 chars for a space to break at, avoiding mid-word cuts.
    const WORD_BREAK_WINDOW: usize = 30;
    let search_start = cut.char_indices()
        .rev()
        .nth(WORD_BREAK_WINDOW - 1)
        .map_or(0, |(i, _)| i);
    if let Some(pos) = cut[search_start..].rfind(' ') {
        format!("{}...", &cut[..search_start + pos])
    } else {
        format!("{}...", cut)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn content_block_unknown_roundtrip() {
        let json_data = json!({
            "type": "future_type",
            "data": "some data",
            "nested": {"key": "value"}
        });
        let block: ContentBlock = serde_json::from_str(&json_data.to_string()).unwrap();
        match &block {
            ContentBlock::Unknown(v) => {
                assert_eq!(v.get("type").unwrap(), "future_type");
            }
            _ => panic!("Expected Unknown variant"),
        }
        let round_trip: serde_json::Value = serde_json::from_str(&serde_json::to_string(&block).unwrap()).unwrap();
        assert_eq!(round_trip, json_data);
    }

    #[test]
    fn thinking_sig_roundtrip() {
        let json_data = json!({ "type": "thinking", "thinking": "let me reason", "sig": "abc123" });
        let block: ContentBlock = serde_json::from_str(&json_data.to_string()).unwrap();
        if let ContentBlock::Thinking { thinking, sig } = &block {
            assert_eq!(thinking, "let me reason");
            assert_eq!(sig.as_deref(), Some("abc123"));
        } else {
            panic!("Expected Thinking variant");
        }
    }
}
