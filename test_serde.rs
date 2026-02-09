use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
        #[serde(flatten)]
        extra: HashMap<String, serde_json::Value>,
    },
    #[serde(untagged)]
    Unknown(serde_json::Value),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Message {
    pub id: String,
    pub content: Vec<ContentBlock>,
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

fn main() {
    // 1. Test unknown content block type
    let json_unknown = r#"{"type": "future_type", "data": "some data"}"#;
    let block: ContentBlock = serde_json::from_str(json_unknown).unwrap();
    println!("Unknown block: {:?}", block);
    let round_trip = serde_json::to_string(&block).unwrap();
    println!("Round trip unknown: {}", round_trip);

    // 2. Test extra fields in known block
    let json_text_extra = r#"{"type": "text", "text": "hello", "sig": "signature"}"#;
    let block_text: ContentBlock = serde_json::from_str(json_text_extra).unwrap();
    println!("Text with extra: {:?}", block_text);
    let round_trip_text = serde_json::to_string(&block_text).unwrap();
    println!("Round trip text: {}", round_trip_text);

    // 3. Test extra fields in message
    let json_msg_extra = r#"{"id": "msg1", "content": [{"type": "text", "text": "hi"}], "foo": "bar"}"#;
    let msg: Message = serde_json::from_str(json_msg_extra).unwrap();
    println!("Message with extra: {:?}", msg);
    let round_trip_msg = serde_json::to_string(&msg).unwrap();
    println!("Round trip message: {}", round_trip_msg);
}
