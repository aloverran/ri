//! Core data types for ri's message model.
//!
//! Three layers, inspired by git's object model:
//!
//! - **Message**: Immutable content blob (role + content blocks). Like a git blob.
//! - **Context**: Ordered list of message IDs. Like a git tree. Cheap to clone
//!   (copy-on-write via shared message pool). Treat it like a value.
//! - **Step**: A point in the history DAG. Records a context snapshot, parent
//!   steps, and metadata. Like a git commit.
//!
//! Sessions (defined in store.rs) are named pointers to steps -- like git branches.

use std::borrow::Borrow;
use std::fmt;

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

// -- ID newtypes --

macro_rules! define_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn new(s: impl Into<String>) -> Self {
                $name(s.into())
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }

        impl Borrow<str> for $name {
            fn borrow(&self) -> &str {
                &self.0
            }
        }

        impl From<String> for $name {
            fn from(s: String) -> Self {
                $name(s)
            }
        }

        impl From<&str> for $name {
            fn from(s: &str) -> Self {
                $name(s.to_string())
            }
        }
    };
}

define_id!(
    /// Unique identifier for a message in the pool.
    MessageId
);

define_id!(
    /// Unique identifier for a step in the history DAG.
    StepId
);

define_id!(
    /// File-stem identifier for a session (e.g. "2026-02-28_120000_fix-login").
    SessionId
);

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

// -- Message --

/// Immutable content blob. The atomic unit of the system.
///
/// Messages live in the pool and are referenced by ID from contexts.
/// They carry no provenance -- that belongs to the Step that introduced them.
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

        let fixed_len = role_tag.len() + 3;
        let content_budget = 800usize.saturating_sub(fixed_len);
        let content_summary = summarize_blocks(&self.content, content_budget);
        format!("[{}] {}", role_tag, content_summary)
    }
}

// -- Context --

/// An ordered list of message references. Represents what the LLM sees.
///
/// Under the hood, just a Vec of message IDs pointing into the pool.
/// Cloning is cheap (small vec of short strings). Treat it like a value type.
///
/// This is the copy-on-write building block: two contexts can share most of
/// their message IDs, and creating a variant (append, replace, subset) is
/// a simple Vec operation on the ID list.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Context {
    pub messages: Vec<MessageId>,
}

impl Context {
    pub fn new() -> Self {
        Context { messages: Vec::new() }
    }

    pub fn from_ids(ids: Vec<MessageId>) -> Self {
        Context { messages: ids }
    }

    /// New context with a message appended.
    pub fn append(&self, id: impl Into<MessageId>) -> Self {
        let mut msgs = self.messages.clone();
        msgs.push(id.into());
        Context { messages: msgs }
    }

    /// New context with several messages appended.
    pub fn extend(&self, ids: impl IntoIterator<Item = MessageId>) -> Self {
        let mut msgs = self.messages.clone();
        msgs.extend(ids);
        Context { messages: msgs }
    }

    /// New context with the message at `index` replaced.
    pub fn replace(&self, index: usize, id: impl Into<MessageId>) -> Self {
        let mut msgs = self.messages.clone();
        msgs[index] = id.into();
        Context { messages: msgs }
    }

    /// New context containing only messages in the given range.
    pub fn subset(&self, range: std::ops::Range<usize>) -> Self {
        Context { messages: self.messages[range].to_vec() }
    }

    /// New context with the message at `index` removed.
    pub fn without(&self, index: usize) -> Self {
        let mut msgs = self.messages.clone();
        msgs.remove(index);
        Context { messages: msgs }
    }

    pub fn len(&self) -> usize {
        self.messages.len()
    }

    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &MessageId> {
        self.messages.iter()
    }
}

// -- Step --

/// A point in the history DAG. Records a context snapshot, parent steps,
/// and metadata about how this context was produced.
///
/// Like a git commit: it captures *what* the context looks like at this point
/// and *how* it got here (parents). The meta field carries model info, usage
/// stats, timestamps, or any application-specific data.
///
/// Parent steps form a DAG:
/// - Linear turn: one parent (the previous step)
/// - Compaction: one parent, meta notes it was compacted
/// - Merge: multiple parents
/// - Root: no parents (initial context)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Step {
    pub id: StepId,
    pub context: Context,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parents: Vec<StepId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<serde_json::Value>,
}

// -- Pool --

/// The shared object store. Messages and steps live here, referenced by ID.
///
/// The pool doesn't know about sessions or files. It's a bag of objects
/// with lookup by ID. The store layer populates it from disk and writes
/// new objects to session files.
pub struct Pool {
    messages: HashMap<MessageId, Message>,
    steps: HashMap<StepId, Step>,
}

impl Pool {
    pub fn new() -> Self {
        Pool {
            messages: HashMap::new(),
            steps: HashMap::new(),
        }
    }

    // -- Messages --

    pub fn put_message(&mut self, msg: Message) {
        assert!(!msg.id.as_str().is_empty(), "Message ID must not be empty (role={:?})", msg.role);
        self.messages.insert(msg.id.clone(), msg);
    }

    pub fn get_message(&self, id: &str) -> Option<&Message> {
        self.messages.get(id)
    }

    /// Resolve an ordered list of message IDs to their messages.
    /// Silently skips IDs not found in the pool.
    pub fn resolve(&self, ids: &[MessageId]) -> Vec<&Message> {
        ids.iter().filter_map(|id| self.messages.get(id)).collect()
    }

    /// Resolve a context to its messages.
    pub fn resolve_context(&self, ctx: &Context) -> Vec<&Message> {
        self.resolve(&ctx.messages)
    }

    pub fn message_count(&self) -> usize {
        self.messages.len()
    }

    // -- Steps --

    pub fn put_step(&mut self, step: Step) {
        assert!(!step.id.as_str().is_empty(), "Step ID must not be empty");
        self.steps.insert(step.id.clone(), step);
    }

    pub fn get_step(&self, id: &str) -> Option<&Step> {
        self.steps.get(id)
    }

    pub fn step_count(&self) -> usize {
        self.steps.len()
    }

    /// Walk parent steps from the given step ID, collecting the full
    /// ancestry chain (breadth-first). Useful for history views.
    pub fn step_ancestry(&self, id: &str) -> Vec<&Step> {
        let mut visited = std::collections::HashSet::new();
        let mut queue = std::collections::VecDeque::new();
        let mut result = Vec::new();

        queue.push_back(id.to_string());
        while let Some(current) = queue.pop_front() {
            if !visited.insert(current.clone()) { continue; }
            if let Some(step) = self.steps.get(current.as_str()) {
                result.push(step);
                for parent_id in &step.parents {
                    if !visited.contains(parent_id.as_str()) {
                        queue.push_back(parent_id.to_string());
                    }
                }
            }
        }
        result
    }
}

impl Default for Pool {
    fn default() -> Self {
        Self::new()
    }
}

// -- ID generation --

/// Generate a globally unique ID.
pub fn gen_id() -> String {
    Uuid::new_v4().simple().to_string()
}

/// Generate a session prefix from name + random suffix, used to create
/// human-readable but unique IDs within a session file.
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
    if s.len() <= max_chars {
        if s.is_ascii() {
            return s.to_string();
        }
    }
    let char_count = s.chars().count();
    if char_count <= max_chars {
        return s.to_string();
    }
    let cut: String = s.chars().take(max_chars).collect();
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
    fn context_append_does_not_mutate_original() {
        let c1 = Context::from_ids(vec!["a".into(), "b".into()]);
        let c2 = c1.append("c");
        assert_eq!(c1.len(), 2);
        assert_eq!(c2.len(), 3);
        assert_eq!(c2.messages, vec![MessageId::from("a"), MessageId::from("b"), MessageId::from("c")]);
    }

    #[test]
    fn context_replace() {
        let c1 = Context::from_ids(vec!["a".into(), "b".into(), "c".into()]);
        let c2 = c1.replace(1, "x");
        assert_eq!(c2.messages, vec![MessageId::from("a"), MessageId::from("x"), MessageId::from("c")]);
        assert_eq!(c1.messages, vec![MessageId::from("a"), MessageId::from("b"), MessageId::from("c")]);
    }

    #[test]
    fn context_without() {
        let c1 = Context::from_ids(vec!["a".into(), "b".into(), "c".into()]);
        let c2 = c1.without(1);
        assert_eq!(c2.messages, vec![MessageId::from("a"), MessageId::from("c")]);
    }

    #[test]
    fn context_subset() {
        let c1 = Context::from_ids(vec!["a".into(), "b".into(), "c".into(), "d".into()]);
        let c2 = c1.subset(1..3);
        assert_eq!(c2.messages, vec![MessageId::from("b"), MessageId::from("c")]);
    }

    #[test]
    fn pool_resolve_context() {
        let mut pool = Pool::new();
        pool.put_message(Message { id: "m1".into(), role: Role::User, content: vec![ContentBlock::text("hello")], meta: None });
        pool.put_message(Message { id: "m2".into(), role: Role::Assistant, content: vec![ContentBlock::text("hi")], meta: None });

        let ctx = Context::from_ids(vec!["m1".into(), "m2".into(), "m_missing".into()]);
        let resolved = pool.resolve_context(&ctx);
        assert_eq!(resolved.len(), 2); // m_missing silently skipped
        assert_eq!(resolved[0].id, MessageId::from("m1"));
        assert_eq!(resolved[1].id, MessageId::from("m2"));
    }

    #[test]
    fn step_ancestry_linear() {
        let mut pool = Pool::new();
        pool.put_step(Step { id: "s1".into(), context: Context::new(), parents: vec![], meta: None });
        pool.put_step(Step { id: "s2".into(), context: Context::new(), parents: vec!["s1".into()], meta: None });
        pool.put_step(Step { id: "s3".into(), context: Context::new(), parents: vec!["s2".into()], meta: None });

        let ancestry = pool.step_ancestry("s3");
        let ids: Vec<&str> = ancestry.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["s3", "s2", "s1"]);
    }

    #[test]
    fn step_ancestry_merge() {
        let mut pool = Pool::new();
        pool.put_step(Step { id: "s1".into(), context: Context::new(), parents: vec![], meta: None });
        pool.put_step(Step { id: "s2".into(), context: Context::new(), parents: vec![], meta: None });
        pool.put_step(Step { id: "s3".into(), context: Context::new(), parents: vec!["s1".into(), "s2".into()], meta: None });

        let ancestry = pool.step_ancestry("s3");
        assert_eq!(ancestry.len(), 3);
    }

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
