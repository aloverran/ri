//! Chat session facet. Attached to a `ri::Ref` to carry the session-
//! level display and navigation state that every chat-style application
//! (ri-web, ri-cli) needs.
//!
//! `ChatFacet` is the shape of "a ri session"; the `Ref` is the branch
//! pointer. Every chat ref is expected to carry a chat facet under
//! `meta.chat`; legacy Session lines from older data are translated into
//! the same shape on load so callers never need a compatibility branch.

use ri::{Context, ContextId, Facet, HasMeta, Message, Pool, Ref, RefId, Store};
use serde::{Deserialize, Serialize};

/// Application-level state for a chat session. The title drives the
/// session list; created_at drives the sort order; cwd + host drive
/// where the session's tools run; parent expresses sub-agent lineage.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatFacet {
    pub title: String,
    pub created_at: String,
    #[serde(default)]
    pub cwd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// Parent ref, if this session was spawned by another.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<RefId>,
}

impl Facet for ChatFacet {
    const KEY: &'static str = "chat";
}

/// Create a new chat session: write an empty root context and a ref
/// carrying the given `ChatFacet`, both into the given store's mount.
/// Returns the ref.
///
/// This is the one common multi-write operation that chat applications
/// need; written here to keep call sites declarative.
pub fn create(store: &Store, facet: ChatFacet) -> eyre::Result<Ref> {
    let root = Context::new(Vec::new(), Vec::new(), None);
    store.write_context(&root)?;
    let r = Ref::new(root.id.clone(), None).with_facet(&facet)?;
    store.write_ref(&r)?;
    Ok(r)
}

/// Resolve the ref's head to the list of messages in its head context.
pub fn head_messages(store: &Store, id: &RefId) -> Vec<Message> {
    let Some(ctx) = store.head_context(id) else { return Vec::new(); };
    store.pool().resolve(&ctx.messages)
}

/// Advance a ref to a new head. The previous ref is read, its head
/// swapped, and the updated ref written as a new line.
pub fn set_head(store: &Store, id: &RefId, head: ContextId) -> eyre::Result<Ref> {
    let current = store.get_ref(id)
        .ok_or_else(|| eyre::eyre!("ref [{}] not found", id))?;
    let next = current.with_head(head);
    store.write_ref(&next)?;
    Ok(next)
}

/// Build a new context whose parent is the ref's current head, then
/// advance the ref to point at it. The common pattern at every workflow
/// boundary (turn complete, tool cycle done, bank entry written).
///
/// Returns the new context. Prior head becomes the new context's parent,
/// so the DAG grows forward without losing history.
pub fn advance_head(
    store: &Store,
    id: &RefId,
    messages: Vec<ri::MessageId>,
    meta: Option<serde_json::Value>,
) -> eyre::Result<Context> {
    let current = store.get_ref(id)
        .ok_or_else(|| eyre::eyre!("ref [{}] not found", id))?;
    let ctx = Context::new(messages, vec![current.head.clone()], meta);
    store.write_context(&ctx)?;
    let next = current.with_head(ctx.id.clone());
    store.write_ref(&next)?;
    Ok(ctx)
}

/// Mutate a ref's chat facet in place and persist. Missing or malformed
/// facets fall back to a default so first-write works on legacy refs.
pub fn update_facet(
    store: &Store,
    id: &RefId,
    edit: impl FnOnce(&mut ChatFacet),
) -> eyre::Result<Ref> {
    let current = store.get_ref(id)
        .ok_or_else(|| eyre::eyre!("ref [{}] not found", id))?;
    let mut chat = current.facet::<ChatFacet>()
        .and_then(|r| r.ok())
        .unwrap_or(ChatFacet {
            title: String::new(),
            created_at: String::new(),
            cwd: String::new(),
            host: None,
            parent: None,
        });
    edit(&mut chat);
    let next = current.with_facet(&chat)?;
    store.write_ref(&next)?;
    Ok(next)
}

/// Pull the chat facet out of a ref, logging and returning `None` when
/// the payload is malformed. A parse failure is a schema bug -- not a
/// recovered abnormal -- so it logs at `error!`. Use this everywhere a
/// chat app reads a ref's display state.
pub fn read_facet(r: &Ref) -> Option<ChatFacet> {
    match r.facet::<ChatFacet>() {
        Some(Ok(f)) => Some(f),
        Some(Err(e)) => {
            tracing::error!("ref [{}] has malformed chat facet: {}", r.id, e);
            None
        }
        None => None,
    }
}

/// Every ref in the pool that carries a `ChatFacet`, paired with the
/// parsed facet for downstream sorting/filtering. The chat-app filter
/// equivalent of "what's the session list" -- since refs are a global
/// atom, this is a pool-level query, not a per-mount one.
///
/// Refs whose meta has the `chat` key but doesn't parse drop out via
/// `read_facet`'s logging path; this list is the recovered subset.
pub fn list_chat_refs(pool: &Pool) -> Vec<(Ref, ChatFacet)> {
    pool.refs()
        .into_iter()
        .filter_map(|r| {
            let facet = read_facet(&r)?;
            Some((r, facet))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `ri-core`'s loader synthesises legacy `Session` lines under the
    /// literal key `"chat"`. If `ChatFacet::KEY` ever drifts away from
    /// that, legacy refs lose their facet on load. This assertion makes
    /// the divergence a build failure rather than a silent regression.
    #[test]
    fn chat_facet_key_matches_legacy_loader() {
        assert_eq!(ChatFacet::KEY, "chat");
    }
}
