use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

// -- Role --

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

    pub fn tool_result_text(tool_use_id: impl Into<String>, text: impl Into<String>, is_error: bool) -> Self {
        ContentBlock::ToolResult {
            tool_use_id: tool_use_id.into(),
            content: vec![ContentBlock::text(text)],
            is_error,
            details: None,
        }
    }

    pub fn tool_result_with_details(tool_use_id: impl Into<String>, text: impl Into<String>, is_error: bool, details: Option<serde_json::Value>) -> Self {
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

    /// Short human-readable summary of this block, targeting 500-1000 chars.
    /// Intended for git-log style session history views.
    pub fn summarize(&self) -> String {
        match self {
            ContentBlock::Text { text } => {
                truncate_with_ellipsis(text, 800)
            }
            ContentBlock::Thinking { thinking, .. } => {
                let body = truncate_with_ellipsis(thinking, 760);
                format!("[thinking] {}", body)
            }
            ContentBlock::Image { media_type, .. } => {
                format!("[image: {}]", media_type)
            }
            ContentBlock::ToolUse { name, input, .. } => {
                let input_str = input.to_string();
                let body = truncate_with_ellipsis(&input_str, 780 - name.len());
                format!("[tool: {}] {}", name, body)
            }
            ContentBlock::ToolResult { is_error, content, .. } => {
                let tag = if *is_error { "[tool error]" } else { "[tool result]" };
                // Flatten inner text blocks into one string.
                let inner: String = content.iter().map(|b| match b {
                    ContentBlock::Text { text } => text.as_str(),
                    _ => "",
                }).collect::<Vec<_>>().join(" ");
                let body = truncate_with_ellipsis(&inner, 780);
                format!("{} {}", tag, body)
            }
            ContentBlock::Error { message } => {
                let body = truncate_with_ellipsis(message, 790);
                format!("[error] {}", body)
            }
            ContentBlock::Unknown(v) => {
                let s = v.to_string();
                truncate_with_ellipsis(&s, 800)
            }
        }
    }
}

// -- Usage --

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

// -- Provenance --

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Provenance {
    pub input: Vec<String>,
    pub model: String,
    pub ts: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
}

// -- Message: the single entity type --

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub id: String,
    pub role: Role,
    pub content: Vec<ContentBlock>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<Provenance>,
    /// Freeform metadata for plugins and higher layers to attach to messages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<serde_json::Value>,
}

impl Message {
    /// Short human-readable summary for git-log style session views.
    /// Includes provenance (model, timestamp, input count) and full usage stats
    /// followed by truncated content block summaries, targeting ~500-1000 chars total.
    pub fn summarize(&self) -> String {
        let role_tag = match self.role {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
        };

        // Build the provenance/usage header if present.
        // This is compact and fixed-size, so it always fits.
        let header = if let Some(prov) = &self.provenance {
            let usage_part = match &prov.usage {
                Some(u) => format!(
                    " | in:{} out:{} cache_r:{} cache_w:{}",
                    u.input_tokens, u.output_tokens, u.cache_read_tokens, u.cache_write_tokens,
                ),
                None => String::new(),
            };
            format!(
                " ({}{} | {} inputs | {})",
                prov.model, usage_part, prov.input.len(), prov.ts,
            )
        } else {
            String::new()
        };

        if self.content.is_empty() {
            return format!("[{}]{} (empty)", role_tag, header);
        }

        // Content budget: total target minus the fixed parts.
        let fixed_len = role_tag.len() + header.len() + 3; // "[" + "]" + " "
        let content_budget = 800usize.saturating_sub(fixed_len);

        let content_summary = summarize_blocks(&self.content, content_budget);
        format!("[{}]{} {}", role_tag, header, content_summary)
    }
}

// -- MessagePool --

pub struct MessagePool {
    messages: HashMap<String, Message>,
}

impl MessagePool {
    pub fn new() -> Self {
        MessagePool { messages: HashMap::new() }
    }

    pub fn put(&mut self, msg: Message) {
        assert!(!msg.id.is_empty(), "Message ID must not be empty (role={:?})", msg.role);
        self.messages.insert(msg.id.clone(), msg);
    }

    pub fn get(&self, id: &str) -> Option<&Message> {
        self.messages.get(id)
    }

    pub fn resolve_existing(&self, ids: &[String]) -> Vec<&Message> {
        ids.iter().filter_map(|id| self.messages.get(id.as_str())).collect()
    }

    pub fn len(&self) -> usize {
        self.messages.len()
    }

    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &Message)> {
        self.messages.iter()
    }

    /// All messages whose provenance.input contains the given ID.
    pub fn derived_from(&self, id: &str) -> Vec<&Message> {
        self.messages.values()
            .filter(|m| {
                m.provenance.as_ref()
                    .is_some_and(|p| p.input.iter().any(|i| i == id))
            })
            .collect()
    }

    /// Walk provenance.input recursively to find all ancestor messages.
    pub fn ancestors(&self, id: &str) -> Vec<&Message> {
        let mut visited = std::collections::HashSet::new();
        let mut stack = vec![id.to_string()];
        let mut result = Vec::new();

        while let Some(current) = stack.pop() {
            if !visited.insert(current.clone()) { continue; }
            if let Some(msg) = self.messages.get(&current) {
                if current != id {
                    result.push(msg);
                }
                if let Some(prov) = &msg.provenance {
                    for input_id in &prov.input {
                        if !visited.contains(input_id) {
                            stack.push(input_id.clone());
                        }
                    }
                }
            }
        }
        result
    }

    /// All derived messages (messages with provenance).
    pub fn derived(&self) -> Vec<&Message> {
        self.messages.values()
            .filter(|m| m.provenance.is_some())
            .collect()
    }

    /// All authored messages (messages without provenance).
    pub fn authored(&self) -> Vec<&Message> {
        self.messages.values()
            .filter(|m| m.provenance.is_none())
            .collect()
    }
}

impl Default for MessagePool {
    fn default() -> Self {
        Self::new()
    }
}

// -- ID generation --

/// Generate a globally unique ID.
/// Uses UUID v4 (128-bit random), formatted as a short hex string without dashes.
pub fn gen_id() -> String {
    Uuid::new_v4().simple().to_string()
}

/// Generate a session prefix from name + random suffix, used to create
/// human-readable but unique message IDs within a session.
pub fn gen_session_prefix(name: &str) -> String {
    let slug: String = name.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(6)
        .collect();
    let rand = &Uuid::new_v4().simple().to_string()[..6];
    if slug.is_empty() {
        format!("s_{}", rand)
    } else {
        format!("{}_{}", slug, rand)
    }
}

// -- Summarization helpers --

/// Summarize a list of content blocks into a single string within a char budget.
/// Joins block summaries with " | ", splitting budget evenly across blocks.
fn summarize_blocks(blocks: &[ContentBlock], budget: usize) -> String {
    if blocks.len() == 1 {
        return truncate_with_ellipsis(&blocks[0].summarize(), budget);
    }
    // Reserve 3 chars per separator " | " between blocks.
    let separator_cost = (blocks.len() - 1) * 3;
    let per_block = budget.saturating_sub(separator_cost) / blocks.len();
    let parts: Vec<String> = blocks.iter().map(|b| {
        truncate_with_ellipsis(&b.summarize(), per_block)
    }).collect();
    parts.join(" | ")
}

/// Truncate a string to at most `max_chars` characters, appending "..." if cut.
/// Tries to break at a word boundary within the last 30 chars to avoid mid-word cuts.
fn truncate_with_ellipsis(s: &str, max_chars: usize) -> String {
    let s = s.trim();
    if s.len() <= max_chars {
        // Fast path: ASCII-only short strings (very common).
        if s.is_ascii() {
            return s.to_string();
        }
    }
    let char_count = s.chars().count();
    if char_count <= max_chars {
        return s.to_string();
    }
    let cut: String = s.chars().take(max_chars).collect();
    // Try to find a word boundary (space) in the last 30 chars to cut cleanly.
    // Use char_indices to find a safe byte offset 30 characters from the end.
    let search_start = cut.char_indices()
        .rev()
        .nth(29)
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
    fn test_content_block_unknown_roundtrip() {
        let json_data = json!({
            "type": "future_type",
            "data": "some data",
            "nested": {"key": "value"}
        });
        let json_str = json_data.to_string();
        
        let block: ContentBlock = serde_json::from_str(&json_str).unwrap();
        
        match &block {
            ContentBlock::Unknown(v) => {
                assert_eq!(v.get("type").unwrap(), "future_type");
                assert_eq!(v.get("data").unwrap(), "some data");
            }
            _ => panic!("Expected Unknown variant, got {:?}", block),
        }
        
        let round_trip = serde_json::to_string(&block).unwrap();
        let back: serde_json::Value = serde_json::from_str(&round_trip).unwrap();
        assert_eq!(back, json_data);
    }

    #[test]
    fn test_thinking_sig_roundtrip() {
        let json_data = json!({
            "type": "thinking",
            "thinking": "let me reason",
            "sig": "abc123"
        });
        let block: ContentBlock = serde_json::from_str(&json_data.to_string()).unwrap();
        if let ContentBlock::Thinking { thinking, sig } = &block {
            assert_eq!(thinking, "let me reason");
            assert_eq!(sig.as_deref(), Some("abc123"));
        } else {
            panic!("Expected Thinking variant");
        }
        let round_trip: serde_json::Value = serde_json::from_str(&serde_json::to_string(&block).unwrap()).unwrap();
        assert_eq!(round_trip, json_data);
    }

    #[test]
    fn test_thinking_no_sig() {
        let json_data = json!({ "type": "thinking", "thinking": "hello" });
        let block: ContentBlock = serde_json::from_str(&json_data.to_string()).unwrap();
        if let ContentBlock::Thinking { sig, .. } = &block {
            assert!(sig.is_none());
        } else {
            panic!("Expected Thinking variant");
        }
        // sig should be omitted from serialization when None
        let serialized = serde_json::to_string(&block).unwrap();
        assert!(!serialized.contains("sig"));
    }

    #[test]
    fn test_tool_result_details_roundtrip() {
        let json_data = json!({
            "type": "tool_result",
            "toolUseId": "call_1",
            "content": [{"type": "text", "text": "ok"}],
            "is_error": false,
            "details": {"exit_code": 0, "lines": 42}
        });
        let block: ContentBlock = serde_json::from_str(&json_data.to_string()).unwrap();
        if let ContentBlock::ToolResult { details, .. } = &block {
            let d = details.as_ref().unwrap();
            assert_eq!(d["exit_code"], 0);
            assert_eq!(d["lines"], 42);
        } else {
            panic!("Expected ToolResult variant");
        }
        let round_trip: serde_json::Value = serde_json::from_str(&serde_json::to_string(&block).unwrap()).unwrap();
        assert_eq!(round_trip, json_data);
    }

    #[test]
    fn test_tool_result_no_details() {
        let block = ContentBlock::tool_result_text("call_1", "done", false);
        let serialized = serde_json::to_string(&block).unwrap();
        assert!(!serialized.contains("details"));
    }

    #[test]
    fn test_content_block_no_type() {
        let json_data = json!({
            "data": "no type here"
        });
        
        let res: Result<ContentBlock, _> = serde_json::from_str(&json_data.to_string());
        assert!(res.is_ok(), "Should match Unknown even if type is missing. Error: {:?}", res.err());
        if let ContentBlock::Unknown(v) = res.unwrap() {
            assert_eq!(v.get("data").unwrap(), "no type here");
        } else {
            panic!("Expected Unknown");
        }
    }
}
