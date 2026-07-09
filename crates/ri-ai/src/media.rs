//! Provider-agnostic blob resolution: the bridge from a content-addressed
//! `ContentBlock::Blob` (just `{media_type, hash, size}`) to bytes a provider
//! can actually transmit.
//!
//! The norm is to **inject the bytes**. A provider's async `stream()` runs a
//! capability-aware resolution pass before building the request body: it walks
//! every `Blob` a request carries (including those nested inside a
//! `ToolResult`), reads the bytes off the Tokio path for the ones this model
//! can take, and records a [`Resolved`] per hash. The **synchronous**
//! body-builders then look each hash up in the resulting [`ResolvedMap`] and
//! emit provider JSON with zero I/O.
//!
//! A [`Resolved::Placeholder`] is the rare backstop -- surfaced, never a silent
//! drop -- for a blob this model genuinely can't carry (wrong modality, over a
//! hard size limit, or a Files-API path not yet built in Stage 2).

use std::collections::{HashMap, HashSet};

use base64::Engine;
use ri::{BlobHash, Blobs, ContentBlock, Message};

/// What a single blob resolved to for one provider request.
pub enum Resolved {
    /// Send the bytes inline as base64.
    Inline { media_type: String, b64: String },
    /// Reference an already-uploaded file by URI (Gemini Files API, Stage 3).
    FileUri { media_type: String, uri: String },
    /// Carry a descriptive text instead -- the blob can't reach this model.
    Placeholder(String),
}

/// Per-request map from a blob's content address to how it was resolved.
pub type ResolvedMap = HashMap<BlobHash, Resolved>;

/// Read a blob's raw bytes off the Tokio path (`spawn_blocking` around the
/// synchronous `Blobs::get`). `None` if the blob is missing or the read
/// failed -- callers fall back to a placeholder.
pub async fn read_blob_bytes(blobs: &Blobs, hash: &BlobHash) -> Option<Vec<u8>> {
    let blobs = blobs.clone();
    let hash = hash.clone();
    tokio::task::spawn_blocking(move || blobs.get(&hash))
        .await
        .ok()?
        .ok()?
}

/// Standard base64 of a byte slice, the form every provider transmits inline media in.
pub fn encode_b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// A blob's bytes, base64-encoded for inline transmission. `None` when the
/// blob is missing or unreadable -- callers fall back to a placeholder.
pub async fn read_blob_b64(blobs: &Blobs, hash: &BlobHash) -> Option<String> {
    read_blob_bytes(blobs, hash).await.map(|b| encode_b64(&b))
}

/// Pixel dimensions of a raster image, read from its header alone -- no full
/// decode, no pixel buffer (this is the `imagesize` crate's whole purpose).
/// `None` when the bytes are not a measurable image; a provider decides for
/// itself what an unmeasurable image means against its own limits.
pub fn image_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    imagesize::blob_size(bytes)
        .ok()
        .map(|s| (s.width as u32, s.height as u32))
}

/// Every distinct `Blob` referenced by a message list, including those nested
/// inside a `ToolResult.content`, de-duplicated by hash. Returned as
/// `(media_type, hash, size)` so a provider's resolution pass can apply its
/// capability rule without re-walking the tree.
pub fn collect_blobs(messages: &[Message]) -> Vec<(String, BlobHash, u64)> {
    fn walk(blocks: &[ContentBlock], out: &mut Vec<(String, BlobHash, u64)>, seen: &mut HashSet<BlobHash>) {
        for b in blocks {
            match b {
                ContentBlock::Blob { media_type, hash, size, .. } => {
                    if seen.insert(hash.clone()) {
                        out.push((media_type.clone(), hash.clone(), *size));
                    }
                }
                ContentBlock::ToolResult { content, .. } => walk(content, out, seen),
                _ => {}
            }
        }
    }
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for m in messages {
        walk(&m.content, &mut out, &mut seen);
    }
    out
}

/// Short human byte-size for placeholder text (B / KB / MB).
pub fn human_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{}B", bytes)
    } else if bytes < 1024 * 1024 {
        format!("{:.1}KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1}MB", bytes as f64 / (1024.0 * 1024.0))
    }
}
