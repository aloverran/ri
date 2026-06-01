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
/// where the session's tools run; parent expresses sub-agent lineage;
/// pinned sessions sort to the top of the session list view.
///
/// This facet is shared across chat-style applications (ri-web, ri-cli)
/// and is intentionally narrow: feature-specific per-session state
/// (auto-listen opt-out, bank consultation, etc) lives in its own
/// sibling facet keyed under `meta`, owned by the crate that consumes
/// it. ri-kit knows nothing about features only one of its consumers
/// uses.
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
    /// Operator-set: pinned sessions float to the top of the session
    /// list view. Used as a "currently working with, not finished"
    /// marker. Default false on existing/legacy refs.
    #[serde(default)]
    pub pinned: bool,
}

impl Facet for ChatFacet {
    const KEY: &'static str = "chat";
}

/// Create a new chat session with a caller-chosen ref id: write an empty
/// root context and a ref carrying the given `ChatFacet`, both into the
/// given store. Returns the ref.
///
/// The id is taken as a parameter (rather than minted internally) so the
/// caller can name the storage segment after it *before* writing -- e.g.
/// a per-family file named after the root session id. `create_with_id`
/// itself attaches no storage policy: it writes to whatever store it is
/// handed, keeping the family/segment decision out of ri-kit.
pub fn create_with_id(store: &Store, id: RefId, facet: ChatFacet) -> eyre::Result<Ref> {
    let root = Context::new(Vec::new(), Vec::new(), None);
    store.write_context(&root)?;
    let r = Ref::with_id(id, root.id.clone()).with_facet(&facet)?;
    store.write_ref(&r)?;
    Ok(r)
}

/// Create a new chat session with a freshly minted ref id. Convenience
/// over [`create_with_id`] for callers that don't need the id up front.
///
/// This is the one common multi-write operation that chat applications
/// need; written here to keep call sites declarative.
pub fn create(store: &Store, facet: ChatFacet) -> eyre::Result<Ref> {
    create_with_id(store, RefId::generate(), facet)
}

/// Resolve the family a ref belongs to: walk its `ChatFacet.parent` chain
/// up to the root and return the root's id. A session family -- a root
/// chat plus everything transitively spawned under it -- is stored in one
/// file named by this id, so a write site that lacks the live parent's
/// store (a cold branch/rewind, say) calls this to find the file to
/// append to.
///
/// The walk is cycle-guarded and stops at the first ref that is unloaded
/// or has no chat-parent. Every parent we create points at a chat ref in
/// the same mount, so the returned id names a file in that mount; the
/// guard only keeps a malformed chain from looping.
pub fn family_segment(pool: &Pool, start: &RefId) -> RefId {
    let mut current = start.clone();
    let mut seen = std::collections::HashSet::new();
    while seen.insert(current.clone()) {
        let Some(r) = pool.get_ref(&current) else { break };
        let Some(parent) = read_facet(&r).and_then(|f| f.parent) else { break };
        current = parent;
    }
    current
}

/// The store a chat ref's family writes through: `store` rebound to the
/// family's segment (the root ancestor's id). This is the single place the
/// "a session family is one file named by its root" policy lives, so every
/// write site -- create, resume, branch, sub-agent, compose -- routes the
/// same way. Centralizing it is the point: the wrong store and the right
/// store are the same type, so an open-coded mistake would compile and only
/// surface as a post-restart data-scatter bug; one helper makes that
/// unrepresentable.
///
/// `anchor` is any member of the family (the ref being written, or its
/// parent for a not-yet-written child). A freshly minted root id that isn't
/// in the pool yet resolves to itself, so this works for creation too.
pub fn family_store(store: &Store, anchor: &RefId) -> eyre::Result<Store> {
    store.segment(family_segment(store.pool(), anchor).as_str())
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
            pinned: false,
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
