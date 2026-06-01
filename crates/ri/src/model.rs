//! Core data model: the three primitive atoms.
//!
//! ri-core is a database. Its primitives are pure in-memory atoms with no
//! knowledge of storage:
//!
//! - **Message**: an immutable content blob (role + content blocks).
//! - **Context**: an immutable ordered list of message references plus
//!   parent links. Contexts form a DAG through their parents.
//! - **Ref**: a mutable named pointer to a context. The branch analog.
//!
//! All three share the same shape: `{id, structural essentials, meta}`.
//! `meta` is an open JSON payload accessed through the `Facet` trait --
//! applications attach their own typed schemas without negotiating with
//! core.
//!
//! The LLM API is `f(context.messages) -> Message`. Everything else is
//! algebra on contexts: creating them, composing them, pointing refs at
//! them.

use std::borrow::Borrow;
use std::collections::HashSet;
use std::fmt;

use serde::{Deserialize, Serialize};
use serde::de::DeserializeOwned;
use uuid::Uuid;

// String newtypes for message, context, and ref identifiers.
// Separate types so the compiler catches mix-ups.

macro_rules! string_id {
    ($name:ident, $prefix:expr, $doc:expr) => {
        #[doc = $doc]
        #[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn new(s: impl Into<String>) -> Self { Self(s.into()) }
            pub fn as_str(&self) -> &str { &self.0 }

            /// Mint a fresh id with this primitive's type prefix.
            pub fn generate() -> Self { Self(format!("{}_{}", $prefix, gen_obj_body())) }
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

        impl From<&String> for $name {
            fn from(s: &String) -> Self { Self(s.clone()) }
        }
    };
}

string_id!(MessageId, "msg", "Unique identifier for a message in the pool. Generated IDs carry a `msg_` type prefix (e.g. `msg_2604_b31b6d48c79e`); the prefix is generation-time convention, leaving the ID opaque to the rest of the system.");
string_id!(ContextId, "ctx", "Unique identifier for a context in the history DAG. Generated IDs carry a `ctx_` type prefix (e.g. `ctx_2604_ef8acc68fb72`); the prefix is generation-time convention, leaving the ID opaque to the rest of the system.");
string_id!(RefId, "ref", "Unique identifier for a ref (named pointer to a context). Generated IDs carry a `ref_` type prefix (e.g. `ref_2604_ef8acc68fb72`). Legacy session files load with their original slug as the RefId; the prefix is generation-time convention, not a parse target.");

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
    /// Build a new message with a freshly minted id.
    pub fn new(role: Role, content: Vec<ContentBlock>, meta: Option<serde_json::Value>) -> Self {
        Self { id: MessageId::generate(), role, content, meta }
    }

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

    /// Full human-readable rendering for readMessage output.
    ///
    /// Unlike `summarize()` (which targets ~150 chars), this shows the complete
    /// text content of every block. Opaque replay blobs are replaced with a
    /// size indicator so they never blow up context windows.
    pub fn display(&self) -> String {
        let role_tag = match self.role {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
        };

        let mut out = format!("MESSAGE {}\nrole: {}\n", self.id, role_tag);

        if let Some(meta) = &self.meta {
            out.push_str("meta: ");
            out.push_str(&serde_json::to_string(meta).unwrap_or_default());
            out.push('\n');
        }

        for block in &self.content {
            out.push('\n');
            display_block(&mut out, block);
        }

        out
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
}

/// Provider-specific data needed to replay a thinking block to its originating model.
///
/// Claude and Gemini produce compact cryptographic signatures (~1-3KB).
/// OpenAI produces an encrypted JSON blob containing the full reasoning
/// content (can be hundreds of KB) which must be sent back verbatim.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum ThinkingReplay {
    /// Compact cryptographic token (Anthropic `signature`, Gemini `thoughtSignature`).
    Signature(String),
    /// Full encrypted reasoning item JSON (OpenAI). Opaque and large.
    Encrypted(String),
}

/// A typed piece of content within a message.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
        /// Provider signature for replaying this block to the originating model.
        /// Gemini: `thoughtSignature`. Anthropic: `signature`. OpenAI: item id.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sig: Option<String>,
    },
    Thinking {
        thinking: String,
        /// Provider-specific replay data. See `ThinkingReplay` for why this
        /// is an enum rather than a plain string.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        replay: Option<ThinkingReplay>,
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
        /// Provider signature for replaying this block to the originating model.
        /// Gemini: `thoughtSignature` (required -- 400 error if omitted).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sig: Option<String>,
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
        /// Structured diagnostic data for UI rendering (model, timestamp, raw API response, etc).
        /// Same pattern as ToolResult's details: carried for the frontend, not sent to the LLM.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        details: Option<serde_json::Value>,
    },
    // Catch-all for unknown block types -- preserves round-trip.
    #[serde(untagged)]
    Unknown(serde_json::Value),
}

impl ContentBlock {
    pub fn text(s: impl Into<String>) -> Self {
        ContentBlock::Text { text: s.into(), sig: None }
    }

    pub fn thinking(s: impl Into<String>) -> Self {
        ContentBlock::Thinking { thinking: s.into(), replay: None }
    }

    pub fn tool_use(id: impl Into<String>, name: impl Into<String>, input: serde_json::Value) -> Self {
        ContentBlock::ToolUse { id: id.into(), name: name.into(), input, sig: None }
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
        ContentBlock::Error { message: s.into(), details: None }
    }

    pub fn error_with_details(s: impl Into<String>, details: serde_json::Value) -> Self {
        ContentBlock::Error { message: s.into(), details: Some(details) }
    }

    /// Text representation of a tool block for contexts where structured
    /// tool protocol can't be emitted (orphaned call/result pairs crossing
    /// provider boundaries). Returns None for non-tool blocks.
    pub fn tool_as_text(&self) -> Option<String> {
        match self {
            ContentBlock::ToolUse { name, input, .. } => {
                Some(format!("[tool call: {}({})]", name, input))
            }
            ContentBlock::ToolResult { content, is_error, .. } => {
                let text: String = content.iter().filter_map(|b| {
                    if let ContentBlock::Text { text, .. } = b { Some(text.as_str()) } else { None }
                }).collect::<Vec<_>>().join("\n");
                if text.is_empty() { return None; }
                let label = if *is_error { "tool error" } else { "tool output" };
                Some(format!("[{}: {}]", label, text))
            }
            _ => None,
        }
    }

    /// Short human-readable summary of this block, targeting ~800 chars.
    pub fn summarize(&self) -> String {
        match self {
            ContentBlock::Text { text, .. } => {
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
                    ContentBlock::Text { text, .. } => text.as_str(),
                    _ => "",
                }).collect::<Vec<_>>().join(" ");
                let body = truncate_with_ellipsis(&inner, SUMMARY_WIDTH - tag.len());
                format!("{tag}{body}")
            }
            ContentBlock::Error { message, .. } => {
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

/// Format a content block for full-content readMessage display.
fn display_block(out: &mut String, block: &ContentBlock) {
    match block {
        ContentBlock::Text { text, .. } => {
            out.push_str("--- text ---\n");
            out.push_str(text);
            out.push('\n');
        }
        ContentBlock::Thinking { thinking, replay } => {
            out.push_str("--- thinking ---\n");
            out.push_str(thinking);
            out.push('\n');
            match replay {
                Some(ThinkingReplay::Signature(s)) => {
                    let size = format_byte_size(s.len());
                    out.push_str(&format!("(replay: signature, {})\n", size));
                }
                Some(ThinkingReplay::Encrypted(s)) => {
                    let size = format_byte_size(s.len());
                    out.push_str(&format!("(replay: encrypted, {} -- not shown)\n", size));
                }
                None => {}
            }
        }
        ContentBlock::Image { media_type, data } => {
            let size = format_byte_size(data.len());
            out.push_str(&format!("--- image ({}, {}) ---\n", media_type, size));
        }
        ContentBlock::ToolUse { name, input, .. } => {
            out.push_str(&format!("--- tool_use: {} ---\n", name));
            out.push_str(&serde_json::to_string_pretty(input).unwrap_or_default());
            out.push('\n');
        }
        ContentBlock::ToolResult { tool_use_id, content, is_error, .. } => {
            let label = if *is_error { "tool_error" } else { "tool_result" };
            out.push_str(&format!("--- {} (call {}) ---\n", label, tool_use_id));
            for inner in content {
                if let ContentBlock::Text { text, .. } = inner {
                    out.push_str(text);
                    out.push('\n');
                }
            }
        }
        ContentBlock::Error { message, details } => {
            out.push_str(&format!("--- error ---\n{}\n", message));
            if let Some(d) = details {
                out.push_str(&format!("details: {}\n", serde_json::to_string_pretty(d).unwrap_or_default()));
            }
        }
        ContentBlock::Unknown(v) => {
            out.push_str("--- unknown ---\n");
            out.push_str(&serde_json::to_string_pretty(v).unwrap_or_default());
            out.push('\n');
        }
    }
}

fn format_byte_size(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{}B", bytes)
    } else if bytes < 1024 * 1024 {
        format!("{:.1}KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1}MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

// -- Tool pair analysis --

/// Compute which tool call IDs form complete call+result pairs in a message list.
///
/// A ToolUse block in message N depends on a ToolResult block in message N+1.
/// When messages are cherry-picked or forwarded across provider boundaries,
/// these pairs can be split. Provider projection layers use this to decide
/// which tool blocks can be emitted as structured protocol and which must
/// be demoted or skipped.
pub fn complete_tool_pairs<'a>(messages: &'a [Message]) -> HashSet<&'a str> {
    let mut calls = HashSet::new();
    let mut results = HashSet::new();

    for msg in messages {
        for block in &msg.content {
            match block {
                ContentBlock::ToolUse { id, .. } => { calls.insert(id.as_str()); }
                ContentBlock::ToolResult { tool_use_id, .. } => { results.insert(tool_use_id.as_str()); }
                _ => {}
            }
        }
    }

    calls.intersection(&results).copied().collect()
}

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

impl Context {
    /// Build a new context with a freshly minted id.
    pub fn new(
        messages: Vec<MessageId>,
        parents: Vec<ContextId>,
        meta: Option<serde_json::Value>,
    ) -> Self {
        Self { id: ContextId::generate(), messages, parents, meta }
    }
}

/// A named mutable pointer to a context. Refs are the branch analog:
/// where a message/context is immutable content, a ref is a moving
/// target you point at whatever context is "current". The application
/// decides what "current" means.
///
/// Refs are the minimum viable pointer. Everything else -- display name,
/// creation timestamp, cwd, parent ref, host -- lives in `meta` via
/// facets. This keeps the primitive neutral: chat, memory banks, and
/// whatever comes next all attach their own typed payloads without
/// touching core.
///
/// On disk, every write appends a full snapshot line. Last line per
/// RefId wins on load.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Ref {
    pub id: RefId,
    pub head: ContextId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<serde_json::Value>,
}

impl Ref {
    /// Build a new ref with a freshly minted id.
    pub fn new(head: ContextId, meta: Option<serde_json::Value>) -> Self {
        Self { id: RefId::generate(), head, meta }
    }

    /// Build a ref with a caller-chosen id rather than a minted one. Used
    /// when the id must be known before the ref is written -- e.g. naming
    /// a storage segment after it.
    pub fn with_id(id: RefId, head: ContextId) -> Self {
        Self { id, head, meta: None }
    }

    /// Swap the head pointer, keeping id and meta unchanged. Useful in
    /// read-modify-write update flows: `store.get_ref(&id)?.with_head(ctx)`.
    pub fn with_head(mut self, head: ContextId) -> Self {
        self.head = head;
        self
    }

    /// Attach or replace a facet payload.
    pub fn with_facet<F: Facet>(mut self, f: &F) -> Result<Self, serde_json::Error> {
        self.set_facet(f)?;
        Ok(self)
    }
}

/// Typed access layer over `meta`. A facet is a strongly-typed payload
/// an application attaches under its own key -- ri-core never knows or
/// cares what's in there.
///
/// Keys form private namespaces owned by the defining crate. Two
/// applications can attach independent facets to the same atom without
/// negotiating with each other.
pub trait Facet: Serialize + DeserializeOwned + Sized {
    /// Key under `meta` where this facet's data lives.
    const KEY: &'static str;
}

/// Atoms that carry an optional `meta` payload. Messages, contexts, and
/// refs all qualify; typed facet access is via `facet::<F>()` / `set_facet`.
///
/// The `meta` shape on disk is an open JSON object. Facet extraction
/// returns `Option<Result<F, _>>`: outer `None` means the facet key
/// isn't present; `Some(Err)` means the key is present but can't be
/// parsed as `F` (a canary worth surfacing rather than swallowing).
pub trait HasMeta {
    fn meta(&self) -> Option<&serde_json::Value>;
    fn meta_mut(&mut self) -> &mut Option<serde_json::Value>;

    /// Pull a facet out of meta. Returns `None` if the key isn't
    /// present; `Some(Err)` if the key is present but malformed.
    fn facet<F: Facet>(&self) -> Option<Result<F, serde_json::Error>> {
        let v = self.meta()?.get(F::KEY)?;
        Some(serde_json::from_value(v.clone()))
    }

    /// Store a facet under its key, replacing any previous value at
    /// that key. Other keys are preserved.
    fn set_facet<F: Facet>(&mut self, f: &F) -> Result<(), serde_json::Error> {
        let value = serde_json::to_value(f)?;
        let meta = self.meta_mut();
        match meta {
            Some(serde_json::Value::Object(map)) => {
                map.insert(F::KEY.to_string(), value);
            }
            _ => {
                let mut map = serde_json::Map::new();
                map.insert(F::KEY.to_string(), value);
                *meta = Some(serde_json::Value::Object(map));
            }
        }
        Ok(())
    }
}

impl HasMeta for Message {
    fn meta(&self) -> Option<&serde_json::Value> { self.meta.as_ref() }
    fn meta_mut(&mut self) -> &mut Option<serde_json::Value> { &mut self.meta }
}

impl HasMeta for Context {
    fn meta(&self) -> Option<&serde_json::Value> { self.meta.as_ref() }
    fn meta_mut(&mut self) -> &mut Option<serde_json::Value> { &mut self.meta }
}

impl HasMeta for Ref {
    fn meta(&self) -> Option<&serde_json::Value> { self.meta.as_ref() }
    fn meta_mut(&mut self) -> &mut Option<serde_json::Value> { &mut self.meta }
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

/// Build the body shared by all type-prefixed object IDs: `{YYMM}_{12 hex}`.
///
/// The two-digit year + month prefix gives temporal context at a glance.
/// Twelve hex characters (48 bits of randomness) are collision-safe to
/// well beyond 200k objects per month.
fn gen_obj_body() -> String {
    let now = chrono::Utc::now();
    let hex = &Uuid::new_v4().simple().to_string()[..12];
    format!("{}_{}", now.format("%y%m"), hex)
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
    fn thinking_replay_roundtrip() {
        let json_data = json!({
            "type": "thinking",
            "thinking": "let me reason",
            "replay": { "type": "signature", "value": "abc123" }
        });
        let block: ContentBlock = serde_json::from_str(&json_data.to_string()).unwrap();
        if let ContentBlock::Thinking { thinking, replay, .. } = &block {
            assert_eq!(thinking, "let me reason");
            assert!(matches!(replay, Some(ThinkingReplay::Signature(s)) if s == "abc123"));
        } else {
            panic!("Expected Thinking variant");
        }
    }

    #[test]
    fn thinking_encrypted_roundtrip() {
        let json_data = json!({
            "type": "thinking",
            "thinking": "summary only",
            "replay": { "type": "encrypted", "value": "{\"id\":\"item_abc\"}" }
        });
        let block: ContentBlock = serde_json::from_str(&json_data.to_string()).unwrap();
        if let ContentBlock::Thinking { thinking, replay, .. } = &block {
            assert_eq!(thinking, "summary only");
            assert!(matches!(replay, Some(ThinkingReplay::Encrypted(s)) if s.contains("item_abc")));
        } else {
            panic!("Expected Thinking variant");
        }
    }
}
