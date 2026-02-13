use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::id::gen_id;

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
        #[serde(flatten)]
        extra: HashMap<String, serde_json::Value>,
    },
    Thinking {
        thinking: String,
        #[serde(flatten)]
        extra: HashMap<String, serde_json::Value>,
    },
    Image {
        #[serde(rename = "mediaType")]
        media_type: String,
        data: String,
        #[serde(flatten)]
        extra: HashMap<String, serde_json::Value>,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
        #[serde(flatten)]
        extra: HashMap<String, serde_json::Value>,
    },
    ToolResult {
        #[serde(rename = "toolUseId")]
        tool_use_id: String,
        content: Vec<ContentBlock>,
        #[serde(default)]
        is_error: bool,
        #[serde(flatten)]
        extra: HashMap<String, serde_json::Value>,
    },
    // Catch-all for unknown block types -- preserves round-trip.
    #[serde(untagged)]
    Unknown(serde_json::Value),
}

impl ContentBlock {
    pub fn text(s: impl Into<String>) -> Self {
        ContentBlock::Text { text: s.into(), extra: HashMap::new() }
    }

    pub fn thinking(s: impl Into<String>) -> Self {
        ContentBlock::Thinking { thinking: s.into(), extra: HashMap::new() }
    }

    pub fn tool_use(id: impl Into<String>, name: impl Into<String>, input: serde_json::Value) -> Self {
        ContentBlock::ToolUse { id: id.into(), name: name.into(), input, extra: HashMap::new() }
    }

    pub fn tool_result(tool_use_id: impl Into<String>, content: Vec<ContentBlock>, is_error: bool) -> Self {
        ContentBlock::ToolResult { tool_use_id: tool_use_id.into(), content, is_error, extra: HashMap::new() }
    }

    pub fn tool_result_text(tool_use_id: impl Into<String>, text: impl Into<String>, is_error: bool) -> Self {
        ContentBlock::ToolResult {
            tool_use_id: tool_use_id.into(),
            content: vec![ContentBlock::text(text)],
            is_error,
            extra: HashMap::new(),
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<serde_json::Value>,
    // Preserve unknown top-level fields for forward compat.
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

impl Message {
    pub fn new(id: String, role: Role, content: Vec<ContentBlock>) -> Self {
        Message { id, role, content, provenance: None, meta: None, extra: HashMap::new() }
    }

    pub fn user(text: impl Into<String>) -> Self {
        Self::new(gen_id(), Role::User, vec![ContentBlock::text(text)])
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
                    .map_or(false, |p| p.input.iter().any(|i| i == id))
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
    fn test_content_block_text_extra_roundtrip() {
        let json_data = json!({
            "type": "text",
            "text": "hello",
            "sig": "signature",
            "meta": {"foo": "bar"}
        });
        let json_str = json_data.to_string();
        
        let block: ContentBlock = serde_json::from_str(&json_str).unwrap();
        
        if let ContentBlock::Text { text, extra } = &block {
            assert_eq!(text, "hello");
            assert_eq!(extra.get("sig").unwrap(), "signature");
            assert_eq!(extra.get("meta").unwrap().get("foo").unwrap(), "bar");
            assert!(!extra.contains_key("type"));
        } else {
            panic!("Expected Text variant");
        }
        
        let round_trip = serde_json::to_string(&block).unwrap();
        let back: serde_json::Value = serde_json::from_str(&round_trip).unwrap();
        assert_eq!(back, json_data);
    }

    #[test]
    fn test_message_extra_roundtrip() {
        let json_data = json!({
            "id": "msg1",
            "role": "user",
            "content": [{"type": "text", "text": "hi"}],
            "unknown_top_field": "val"
        });
        
        let msg: Message = serde_json::from_str(&json_data.to_string()).unwrap();
        assert_eq!(msg.extra.get("unknown_top_field").unwrap(), "val");
        
        let round_trip = serde_json::to_string(&msg).unwrap();
        let back: serde_json::Value = serde_json::from_str(&round_trip).unwrap();
        assert_eq!(back, json_data);
    }

    #[test]
    fn test_content_block_no_type() {
        let json_data = json!({
            "data": "no type here"
        });
        
        let res: Result<ContentBlock, _> = serde_json::from_str(&json_data.to_string());
        // Since the enum is tagged, serde usually expects the tag.
        // But ContentBlock::Unknown is untagged.
        assert!(res.is_ok(), "Should match Unknown even if type is missing. Error: {:?}", res.err());
        if let ContentBlock::Unknown(v) = res.unwrap() {
            assert_eq!(v.get("data").unwrap(), "no type here");
        } else {
            panic!("Expected Unknown");
        }
    }
}
