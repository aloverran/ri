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

    pub fn assistant(text: impl Into<String>) -> Self {
        Self::new(gen_id(), Role::Assistant, vec![ContentBlock::text(text)])
    }

    pub fn system(text: impl Into<String>) -> Self {
        Self::new(gen_id(), Role::System, vec![ContentBlock::text(text)])
    }

    pub fn is_derived(&self) -> bool {
        self.provenance.is_some()
    }
}

// -- Session header (first line of a session file) --

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionHeader {
    pub session: String,
    pub ts: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

// -- Session info for listing --

#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub path: std::path::PathBuf,
    pub name: String,
    pub ts: String,
    pub cwd: Option<String>,
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
