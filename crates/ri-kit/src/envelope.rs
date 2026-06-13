//! Addressed instructions: the `envelope` facet and its pull machinery.
//!
//! A context can declare a destination ref and a verb by carrying an
//! [`Envelope`] facet -- an addressed instruction. Nothing is sent and
//! nothing wakes: the destination's owner discovers pending envelopes at
//! its own safe boundaries, applies each one's instruction, and records
//! the envelope as a parent of its next checkpoint. Delivery and receipt
//! are therefore the same JSONL line, and "has this been applied?" is
//! answered by graph reachability -- the same way git answers containment
//! -- with no watermark, no clock, and no runtime state.
//!
//! Two verbs exist today (see [`Instruction`]): `Merge` weaves the
//! envelope's messages onto the recipient's current head; `Jump`
//! relocates the head onto a target context. Both differ only in the
//! *base* they append the envelope's messages onto -- everything
//! downstream (parent the next checkpoint on `[old_head, base, envelope]`,
//! dedup by reachability) is identical, so jump falls out of merge rather
//! than sitting beside it. The full design lives in
//! `designs/envelope-instructions.md` (generalizing `designs/merge-into.md`).

use std::collections::{HashMap, HashSet, VecDeque};

use ri::{Context, ContextId, Facet, HasMeta, Pool, RefId};
use serde::{Deserialize, Serialize};

/// Marks a context as an instruction addressed to a ref: a delta authored
/// now, applied by the destination's owner at its next safe boundary. The
/// address (`to`) and the verb (`instruction`) live on the context (the
/// envelope), never on its messages, so content can be referenced by any
/// number of other contexts without dragging delivery semantics along.
///
/// `to` and `instruction` are mutually required -- an address with no verb
/// is delivered nowhere, a verb with no address is undeliverable -- so they
/// are two fields of one facet, never two independent facets. The
/// transport/verb knowledge boundary lives in the code: the scan layer
/// ([`pending_envelopes`]) reads only `to`; the apply layer (in the
/// harness loop) matches only `instruction`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Envelope {
    /// The ref whose owner should apply this instruction.
    pub to: RefId,
    /// What the owner should do on pickup.
    pub instruction: Instruction,
}

/// The verb an [`Envelope`] carries. Each instruction selects the *base*
/// context whose messages the envelope's messages are appended onto; the
/// rest of application is verb-independent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Instruction {
    /// Weave the envelope's messages onto the recipient's current head
    /// (base = self). The original `merge_into` behavior.
    Merge,
    /// Relocate the recipient's head onto `target` (base = target), tagging
    /// the prior head as a live snapshot ref first. `target` is an
    /// immutable context id -- crash-idempotent, no skew. "Jump to a ref"
    /// is author-time sugar that resolves the ref to its head context when
    /// the envelope is constructed.
    Jump { target: ContextId },
}

impl Facet for Envelope {
    const KEY: &'static str = "envelope";
}

/// Every context id reachable from `head` through parent links,
/// including `head` itself. This is a chain's delivery ledger: an
/// envelope is pending until its id appears here, and applying it (as a
/// checkpoint parent) is what makes it appear.
///
/// A parent the pool never loaded is treated as reachable and skipped --
/// most commonly an applied envelope whose constructor family was deleted
/// (a blessed operation), so it is logged at `debug`, not surfaced as a
/// canary.
pub fn reachable_contexts(pool: &Pool, head: &ContextId) -> HashSet<ContextId> {
    let _span = tracing::info_span!("envelope_ledger_walk").entered();
    let mut seen: HashSet<ContextId> = HashSet::new();
    let mut queue: VecDeque<ContextId> = VecDeque::new();
    queue.push_back(head.clone());
    while let Some(id) = queue.pop_front() {
        if !seen.insert(id.clone()) {
            continue;
        }
        let Some(ctx) = pool.get_context(&id) else {
            // A parent that the pool never loaded: most commonly an applied
            // envelope whose constructor family was deleted (a blessed
            // operation), so this is normal-noise, not a canary.
            tracing::debug!("reachability walk: context [{}] not in pool; treating as reachable", id);
            continue;
        };
        for p in &ctx.parents {
            if !seen.contains(p) {
                queue.push_back(p.clone());
            }
        }
    }
    seen
}

/// All envelopes addressed to `target` that its chain has not applied
/// yet, in application order. Verb-agnostic: the scan keys only on the
/// envelope's address, never its instruction -- the owner partitions by
/// verb when it applies them.
///
/// Order is topological over parent links *within* the pending set: a
/// constructor that chains envelope B onto envelope A (B lists A as a
/// parent) is guaranteed A-before-B. Unrelated envelopes have no order
/// to honor and sort deterministically by id. A malformed facet is a
/// bug signal, not silently absorbable data -- warned and skipped.
pub fn pending_envelopes(pool: &Pool, target: &RefId, reachable: &HashSet<ContextId>) -> Vec<Context> {
    let _span = tracing::info_span!("envelope_scan").entered();
    let mut pending: Vec<Context> = Vec::new();
    let candidates = pool.find_contexts(|c| {
        c.meta().map_or(false, |m| m.get(Envelope::KEY).is_some())
    });
    for ctx in candidates {
        match ctx.facet::<Envelope>() {
            None => {}
            Some(Err(e)) => {
                tracing::warn!("context [{}] carries a malformed envelope facet: {}", ctx.id, e);
            }
            Some(Ok(env)) => {
                if &env.to == target && !reachable.contains(&ctx.id) {
                    pending.push(ctx);
                }
            }
        }
    }
    topo_sort(pending)
}

/// Topological order over parent edges within the set, independents and
/// ties by context id. Fresh context ids cannot form cycles; if forged
/// ids ever do, the leftover is warned and appended in id order rather
/// than dropped -- delivery degrades to arbitrary order, never to loss.
fn topo_sort(set: Vec<Context>) -> Vec<Context> {
    let ids: HashSet<ContextId> = set.iter().map(|c| c.id.clone()).collect();
    let mut indegree: HashMap<ContextId, usize> = HashMap::new();
    let mut children: HashMap<ContextId, Vec<ContextId>> = HashMap::new();
    let mut by_id: HashMap<ContextId, Context> = HashMap::new();
    for c in set {
        let in_set_parents = c.parents.iter().filter(|p| ids.contains(*p)).count();
        for p in c.parents.iter().filter(|p| ids.contains(*p)) {
            children.entry(p.clone()).or_default().push(c.id.clone());
        }
        indegree.insert(c.id.clone(), in_set_parents);
        by_id.insert(c.id.clone(), c);
    }

    let mut ready: Vec<ContextId> = indegree.iter()
        .filter(|(_, d)| **d == 0)
        .map(|(id, _)| id.clone())
        .collect();
    let mut out: Vec<Context> = Vec::new();
    while !ready.is_empty() {
        ready.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        let next = ready.remove(0);
        for child in children.remove(&next).unwrap_or_default() {
            let d = indegree.get_mut(&child).expect("child indegree present");
            *d -= 1;
            if *d == 0 {
                ready.push(child.clone());
            }
        }
        out.push(by_id.remove(&next).expect("ready context present"));
    }

    if !by_id.is_empty() {
        tracing::warn!(
            "envelope ordering found a parent cycle among {} pending context(s); appending in id order",
            by_id.len()
        );
        let mut rest: Vec<Context> = by_id.into_values().collect();
        rest.sort_by(|a, b| a.id.as_str().cmp(b.id.as_str()));
        out.extend(rest);
    }
    out
}
