//! Addressed contexts: the `merge_into` facet and its pull machinery.
//!
//! A context can declare a destination ref by carrying a `MergeInto`
//! facet -- an envelope. Nothing is sent and nothing wakes: the
//! destination's owner discovers pending envelopes at its own safe
//! boundaries, weaves their messages into its working head, and records
//! each envelope as a parent of its next checkpoint. Delivery and
//! receipt are therefore the same JSONL line, and "has this been
//! merged?" is answered by graph reachability -- the same way git
//! answers containment -- with no watermark, no clock, and no runtime
//! state. The full design lives in `designs/merge-into.md` at the
//! workspace root.

use std::collections::{HashMap, HashSet, VecDeque};

use ri::{Context, ContextId, Facet, HasMeta, Pool, RefId};
use serde::{Deserialize, Serialize};

/// Marks a context as addressed to a ref: an extension authored now, to
/// be applied by the destination's owner at its next safe boundary. The
/// address lives on the context (the envelope), never on its messages,
/// so content can be referenced by any number of other contexts without
/// dragging delivery semantics along.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MergeInto(pub RefId);

impl Facet for MergeInto {
    const KEY: &'static str = "merge_into";
}

/// Every context id reachable from `head` through parent links,
/// including `head` itself. This is a chain's delivery ledger: an
/// envelope is pending until its id appears here, and merging it (as a
/// checkpoint parent) is what makes it appear.
///
/// A parent pointing at a context the pool never loaded is a canary --
/// warned and skipped, never fatal.
pub fn reachable_contexts(pool: &Pool, head: &ContextId) -> HashSet<ContextId> {
    let _span = tracing::info_span!("merge_ledger_walk").entered();
    let mut seen: HashSet<ContextId> = HashSet::new();
    let mut queue: VecDeque<ContextId> = VecDeque::new();
    queue.push_back(head.clone());
    while let Some(id) = queue.pop_front() {
        if !seen.insert(id.clone()) {
            continue;
        }
        let Some(ctx) = pool.get_context(&id) else {
            // A parent that the pool never loaded: most commonly a merged
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

/// All envelopes addressed to `target` that its chain has not merged
/// yet, in merge order.
///
/// Order is topological over parent links *within* the pending set: a
/// constructor that chains envelope B onto envelope A (B lists A as a
/// parent) is guaranteed A-before-B. Unrelated envelopes have no order
/// to honor and sort deterministically by id. A malformed facet is a
/// bug signal, not silently absorbable data -- warned and skipped.
pub fn pending_merges(pool: &Pool, target: &RefId, reachable: &HashSet<ContextId>) -> Vec<Context> {
    let _span = tracing::info_span!("merge_scan").entered();
    let mut pending: Vec<Context> = Vec::new();
    let candidates = pool.find_contexts(|c| {
        c.meta().map_or(false, |m| m.get(MergeInto::KEY).is_some())
    });
    for ctx in candidates {
        match ctx.facet::<MergeInto>() {
            None => {}
            Some(Err(e)) => {
                tracing::warn!("context [{}] carries a malformed merge_into facet: {}", ctx.id, e);
            }
            Some(Ok(MergeInto(dest))) => {
                if &dest == target && !reachable.contains(&ctx.id) {
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
            "merge ordering found a parent cycle among {} pending context(s); appending in id order",
            by_id.len()
        );
        let mut rest: Vec<Context> = by_id.into_values().collect();
        rest.sort_by(|a, b| a.id.as_str().cmp(b.id.as_str()));
        out.extend(rest);
    }
    out
}
