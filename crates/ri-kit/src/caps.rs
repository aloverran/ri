//! Capability grants: the `caps` facet and its attenuation algebra.
//!
//! A capability is the right to run one named tool. A [`CapSet`] is the
//! grant a session runs with, attached to its ref as a facet -- authority
//! rides on identity (the ref), never on content (messages and contexts
//! are freely composable data and must not carry it). The loop's actual
//! tool list is exactly the granted names, resolved against whatever
//! inventory the harness can build.
//!
//! Grants only ever shrink as they flow. One ceiling governs every
//! transfer an agent makes -- minting a forked child's grant, running a
//! continued session, or rewriting another ref's grant:
//!
//! ```text
//!     conveyable = transition(own effective caps)
//! ```
//!
//! [`CapSet::transition`] is one grant generation: unit capabilities pass
//! through unchanged, budgeted capabilities decrement their transfer
//! budget and drop out once it is spent. Presence in the set is what says
//! "I hold this"; the budget says only how many more times it may be
//! handed down -- a holder at budget 0 uses the capability freely but
//! cannot convey it. `runAgent` is the budgeted capability in practice,
//! but this module attaches no meaning to names: which capability carries
//! a budget, and at what root value, is the harness's configuration.
//!
//! This module is a leaf: pure data, no knowledge of tools, loops, or
//! harnesses. Enforcement call sites (`runAgent`, `updateRef`) live in
//! `meta_tools`; root grants and tool assembly live in each harness.

use std::collections::BTreeMap;

use ri::Facet;
use serde::{Deserialize, Serialize};

/// One grant within a [`CapSet`].
///
/// A unit grant (`budget: None`) is plain access, unbounded under
/// [`CapSet::transition`]. A budgeted grant additionally bounds
/// re-granting: the budget counts how many more times the capability may
/// be handed down. Each transition decrements it, and a spent budget
/// (0) drops out of the next transition entirely.
///
/// Unknown fields are rejected so that data written under a different
/// encoding surfaces as a malformed facet -- a loud canary that heals
/// through the harness derivation paths -- rather than silently parsing
/// as an unbounded unit grant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cap {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<u32>,
}

impl Cap {
    /// A plain unit grant.
    pub fn unit() -> Self {
        Cap { budget: None }
    }

    /// A grant with a transfer budget.
    pub fn budgeted(budget: u32) -> Self {
        Cap { budget: Some(budget) }
    }
}

/// The set of capabilities a session runs with: tool name -> grant.
///
/// Stored on a session ref under the `caps` facet key. On disk it reads
/// as `{"exec": {}, "runAgent": {"budget": 1}}`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CapSet(pub BTreeMap<String, Cap>);

impl Facet for CapSet {
    const KEY: &'static str = "caps";
}

impl CapSet {
    /// An empty grant: no tools, a single bare LLM turn when run.
    pub fn none() -> Self {
        CapSet(BTreeMap::new())
    }

    /// A set of unit grants.
    pub fn unit(names: impl IntoIterator<Item = impl Into<String>>) -> Self {
        CapSet(names.into_iter().map(|n| (n.into(), Cap::unit())).collect())
    }

    /// Builder: add or replace one budgeted grant.
    pub fn with_budget(mut self, name: impl Into<String>, budget: u32) -> Self {
        self.0.insert(name.into(), Cap::budgeted(budget));
        self
    }

    /// Builder: add or replace one unit grant.
    pub fn with_unit(mut self, name: impl Into<String>) -> Self {
        self.0.insert(name.into(), Cap::unit());
        self
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn contains(&self, name: &str) -> bool {
        self.0.contains_key(name)
    }

    /// The granted names, sorted. This is the loop's tool selection.
    pub fn names(&self) -> Vec<String> {
        self.0.keys().cloned().collect()
    }

    /// One grant generation: unit grants pass through unchanged; budgeted
    /// grants decrement their transfer budget, dropping out once it is
    /// spent. The result is the most any holder of `self` may convey to
    /// another execution identity.
    pub fn transition(&self) -> CapSet {
        CapSet(
            self.0
                .iter()
                .filter_map(|(name, cap)| match cap.budget {
                    None => Some((name.clone(), Cap::unit())),
                    Some(b) if b >= 1 => Some((name.clone(), Cap::budgeted(b - 1))),
                    Some(_) => None,
                })
                .collect(),
        )
    }

    /// Every way `self` exceeds `other`, as human-readable phrases; empty
    /// means `self` fits within `other`. The order: a grant fits when the
    /// same name is granted at least as strongly -- a unit grant exceeds
    /// any budgeted one (unit is unbounded), and budgets compare
    /// numerically.
    pub fn violations(&self, other: &CapSet) -> Vec<String> {
        let mut out = Vec::new();
        for (name, cap) in &self.0 {
            match (other.0.get(name), cap.budget) {
                (None, _) => out.push(format!("[{}] is not granted", name)),
                (Some(Cap { budget: None }), _) => {}
                (Some(Cap { budget: Some(_) }), None) => {
                    out.push(format!("[{}] is unbounded here but bounded there", name))
                }
                (Some(Cap { budget: Some(b) }), Some(a)) if a > *b => {
                    out.push(format!(
                        "[{}] transfer budget {} exceeds budget {}", name, a, b
                    ))
                }
                (Some(Cap { budget: Some(_) }), Some(_)) => {}
            }
        }
        out
    }

    /// Resolve what a request may receive from this holder. `None` asks
    /// for everything conveyable -- the whole [`CapSet::transition`].
    /// `Some(names)` asks for exactly those grants out of it; a name that
    /// is not conveyable is an error that distinguishes "not held at all"
    /// from "held, but with no transfer budget remaining".
    pub fn grant(&self, requested: Option<&[String]>) -> Result<CapSet, String> {
        let conveyable = self.transition();
        let Some(names) = requested else {
            return Ok(conveyable);
        };
        let mut out = BTreeMap::new();
        for name in names {
            match conveyable.0.get(name) {
                Some(cap) => {
                    out.insert(name.clone(), cap.clone());
                }
                None if self.contains(name) => {
                    return Err(format!(
                        "cannot convey [{}]: this session holds it with no transfer \
                         budget remaining",
                        name,
                    ));
                }
                None => {
                    return Err(format!(
                        "cannot convey [{}]: this session's grant does not include it",
                        name
                    ));
                }
            }
        }
        Ok(CapSet(out))
    }

    /// Render for display: `exec, read, runAgent(1)`, or `(none)` when
    /// empty. A grant with shareable budget shows it in parentheses; a
    /// spent budget renders as the bare name, like a unit grant -- held,
    /// nothing left to say about sharing. Names sort naturally via the
    /// underlying map.
    pub fn describe(&self) -> String {
        if self.0.is_empty() {
            return "(none)".to_string();
        }
        self.0
            .iter()
            .map(|(name, cap)| match cap.budget {
                None | Some(0) => name.clone(),
                Some(b) => format!("{}({})", name, b),
            })
            .collect::<Vec<_>>()
            .join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> CapSet {
        CapSet::unit(["exec", "read"]).with_budget("runAgent", 1)
    }

    #[test]
    fn transition_decrements_and_spends() {
        let one = root().transition();
        assert_eq!(one.0["runAgent"], Cap::budgeted(0));
        assert_eq!(one.0["exec"], Cap::unit());

        let two = one.transition();
        assert!(!two.contains("runAgent"), "a spent budget does not convey");
        assert!(two.contains("exec"), "unit grants pass through unchanged");
    }

    #[test]
    fn violations_order_units_and_budgets() {
        let caller = root();
        // Fits: fewer names, lower budget.
        let ok = CapSet::unit(["exec"]).with_budget("runAgent", 0);
        assert!(ok.violations(&caller).is_empty());
        // Exceeds: a name not granted, a budget too high, a unit over a budget.
        let bad = CapSet::unit(["write", "runAgent"]).with_budget("exec", 1);
        let v = bad.violations(&caller);
        assert_eq!(v.len(), 2, "write missing + runAgent unbounded: {:?}", v);
        let high = CapSet::none().with_budget("runAgent", 2);
        assert_eq!(high.violations(&caller).len(), 1);
        // A budgeted grant fits within a unit grant.
        let under = CapSet::none().with_budget("exec", 5);
        assert!(under.violations(&caller).is_empty());
    }

    #[test]
    fn grant_defaults_selects_and_teaches() {
        let caller = root();
        assert_eq!(caller.grant(None).unwrap(), caller.transition());

        let sel = caller.grant(Some(&["exec".into(), "runAgent".into()])).unwrap();
        assert_eq!(sel.0["runAgent"], Cap::budgeted(0));
        assert_eq!(sel.names(), vec!["exec", "runAgent"]);

        let not_held = caller.grant(Some(&["write".into()])).unwrap_err();
        assert!(not_held.contains("does not include"), "{}", not_held);

        let spent = caller.transition().grant(Some(&["runAgent".into()])).unwrap_err();
        assert!(spent.contains("no transfer budget"), "{}", spent);
    }

    #[test]
    fn describe_shows_budget_only_when_shareable() {
        assert_eq!(root().describe(), "exec, read, runAgent(1)");
        assert_eq!(root().transition().describe(), "exec, read, runAgent");
    }

    #[test]
    fn facet_roundtrip_shape_and_foreign_encoding() {
        let json = serde_json::to_value(root()).unwrap();
        assert_eq!(json["exec"], serde_json::json!({}));
        assert_eq!(json["runAgent"], serde_json::json!({"budget": 1}));
        let back: CapSet = serde_json::from_value(json).unwrap();
        assert_eq!(back, root());

        // Data written under a different encoding is malformed, never a
        // silent unit grant.
        let foreign = serde_json::json!({"runAgent": {"level": 2}});
        assert!(serde_json::from_value::<CapSet>(foreign).is_err());
    }
}
