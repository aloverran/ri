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
//! [`CapSet::transition`] is one spawn generation: unit capabilities pass
//! through unchanged, leveled capabilities decrement and drop out at
//! zero. `runAgent` is the leveled capability in practice -- holding it
//! at level n means "n more generations may spawn below me" -- but this
//! module attaches no meaning to names: which capability carries a level,
//! and at what root value, is the harness's configuration.
//!
//! This module is a leaf: pure data, no knowledge of tools, loops, or
//! harnesses. Enforcement call sites (`runAgent`, `updateRef`) live in
//! `meta_tools`; root grants and tool assembly live in each harness.

use std::collections::BTreeMap;

use ri::Facet;
use serde::{Deserialize, Serialize};

/// One grant within a [`CapSet`].
///
/// A unit grant (`level: None`) is plain access, unbounded under
/// [`CapSet::transition`]. A leveled grant additionally bounds re-granting:
/// each transition decrements the level, and a level that would reach zero
/// is dropped rather than stored -- a persisted level is always >= 1.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cap {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<u32>,
}

impl Cap {
    /// A plain unit grant.
    pub fn unit() -> Self {
        Cap { level: None }
    }

    /// A leveled grant.
    pub fn leveled(level: u32) -> Self {
        Cap { level: Some(level) }
    }
}

/// The set of capabilities a session runs with: tool name -> grant.
///
/// Stored on a session ref under the `caps` facet key. On disk it reads
/// as `{"bash": {}, "runAgent": {"level": 2}}`.
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

    /// Builder: add or replace one leveled grant.
    pub fn with_leveled(mut self, name: impl Into<String>, level: u32) -> Self {
        self.0.insert(name.into(), Cap::leveled(level));
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

    /// One spawn generation: unit grants pass through unchanged; leveled
    /// grants decrement, dropping out when the level is exhausted. The
    /// result is the most any holder of `self` may convey to another
    /// execution identity.
    pub fn transition(&self) -> CapSet {
        CapSet(
            self.0
                .iter()
                .filter_map(|(name, cap)| match cap.level {
                    None => Some((name.clone(), Cap::unit())),
                    Some(n) if n >= 2 => Some((name.clone(), Cap::leveled(n - 1))),
                    Some(_) => None,
                })
                .collect(),
        )
    }

    /// Every way `self` exceeds `other`, as human-readable phrases; empty
    /// means `self` fits within `other`. The order: a grant fits when the
    /// same name is granted at least as strongly -- a unit grant exceeds
    /// any leveled one (unit is unbounded), and levels compare numerically.
    pub fn violations(&self, other: &CapSet) -> Vec<String> {
        let mut out = Vec::new();
        for (name, cap) in &self.0 {
            match (other.0.get(name), cap.level) {
                (None, _) => out.push(format!("[{}] is not granted", name)),
                (Some(Cap { level: None }), _) => {}
                (Some(Cap { level: Some(_) }), None) => {
                    out.push(format!("[{}] is unleveled here but bounded there", name))
                }
                (Some(Cap { level: Some(b) }), Some(a)) if a > *b => {
                    out.push(format!("[{}] at level {} exceeds level {}", name, a, b))
                }
                (Some(Cap { level: Some(_) }), Some(_)) => {}
            }
        }
        out
    }

    /// Resolve what a request may receive from this holder. `None` asks
    /// for everything conveyable -- the whole [`CapSet::transition`].
    /// `Some(names)` asks for exactly those grants out of it; a name that
    /// is not conveyable is an error that distinguishes "not held at all"
    /// from "held, but its level does not extend further".
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
                        "cannot convey [{}]: this session holds it at level {}, which does \
                         not extend to another agent",
                        name,
                        self.0[name].level.unwrap_or(0),
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

    /// Render for display: `bash, read, runAgent(2)`, or `(none)` when
    /// empty. Names sort naturally via the underlying map.
    pub fn describe(&self) -> String {
        if self.0.is_empty() {
            return "(none)".to_string();
        }
        self.0
            .iter()
            .map(|(name, cap)| match cap.level {
                None => name.clone(),
                Some(n) => format!("{}({})", name, n),
            })
            .collect::<Vec<_>>()
            .join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> CapSet {
        CapSet::unit(["bash", "read"]).with_leveled("runAgent", 2)
    }

    #[test]
    fn transition_decrements_and_exhausts() {
        let one = root().transition();
        assert_eq!(one.0["runAgent"], Cap::leveled(1));
        assert_eq!(one.0["bash"], Cap::unit());

        let two = one.transition();
        assert!(!two.contains("runAgent"), "level 1 does not extend further");
        assert!(two.contains("bash"), "unit grants pass through unchanged");
    }

    #[test]
    fn violations_order_units_and_levels() {
        let caller = root();
        // Fits: fewer names, lower level.
        let ok = CapSet::unit(["bash"]).with_leveled("runAgent", 1);
        assert!(ok.violations(&caller).is_empty());
        // Exceeds: a name not granted, a level too high, a unit over a level.
        let bad = CapSet::unit(["write", "runAgent"]).with_leveled("bash", 1);
        let v = bad.violations(&caller);
        assert_eq!(v.len(), 2, "write missing + runAgent unleveled: {:?}", v);
        let high = CapSet::none().with_leveled("runAgent", 3);
        assert_eq!(high.violations(&caller).len(), 1);
        // A leveled grant fits within a unit grant.
        let under = CapSet::none().with_leveled("bash", 5);
        assert!(under.violations(&caller).is_empty());
    }

    #[test]
    fn grant_defaults_selects_and_teaches() {
        let caller = root();
        assert_eq!(caller.grant(None).unwrap(), caller.transition());

        let sel = caller.grant(Some(&["bash".into(), "runAgent".into()])).unwrap();
        assert_eq!(sel.0["runAgent"], Cap::leveled(1));
        assert_eq!(sel.names(), vec!["bash", "runAgent"]);

        let not_held = caller.grant(Some(&["write".into()])).unwrap_err();
        assert!(not_held.contains("does not include"), "{}", not_held);

        let exhausted = caller.transition().grant(Some(&["runAgent".into()])).unwrap_err();
        assert!(exhausted.contains("level 1"), "{}", exhausted);
    }

    #[test]
    fn facet_roundtrip_shape() {
        let json = serde_json::to_value(root()).unwrap();
        assert_eq!(json["bash"], serde_json::json!({}));
        assert_eq!(json["runAgent"], serde_json::json!({"level": 2}));
        let back: CapSet = serde_json::from_value(json).unwrap();
        assert_eq!(back, root());
    }
}
