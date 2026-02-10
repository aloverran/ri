use std::collections::HashMap;
use crate::types::Message;

pub struct Pool {
    messages: HashMap<String, Message>,
    // Insertion order for deterministic iteration.
    order: Vec<String>,
}

impl Pool {
    pub fn new() -> Self {
        Pool { messages: HashMap::new(), order: Vec::new() }
    }

    pub fn put(&mut self, msg: Message) {
        assert!(!msg.id.is_empty(), "Message ID must not be empty (role={:?})", msg.role);
        let id = msg.id.clone();
        if !self.messages.contains_key(&id) {
            self.order.push(id.clone());
        }
        self.messages.insert(id, msg);
    }

    pub fn get(&self, id: &str) -> Option<&Message> {
        self.messages.get(id)
    }

    pub fn contains(&self, id: &str) -> bool {
        self.messages.contains_key(id)
    }

    pub fn len(&self) -> usize {
        self.messages.len()
    }

    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    // Iterate in insertion order.
    pub fn iter(&self) -> impl Iterator<Item = &Message> {
        self.order.iter().filter_map(move |id| self.messages.get(id))
    }

    // All messages whose provenance.input contains the given ID.
    pub fn derived_from(&self, id: &str) -> Vec<&Message> {
        self.messages.values()
            .filter(|m| {
                m.provenance.as_ref()
                    .map(|p| p.input.contains(&id.to_string()))
                    .unwrap_or(false)
            })
            .collect()
    }

    // All derived messages (messages with provenance).
    pub fn derived(&self) -> Vec<&Message> {
        self.messages.values()
            .filter(|m| m.is_derived())
            .collect()
    }

    // Resolve a list of message IDs to actual messages.
    // Returns None entries for missing IDs (best-effort).
    pub fn resolve(&self, ids: &[String]) -> Vec<Option<&Message>> {
        ids.iter().map(|id| self.messages.get(id.as_str())).collect()
    }

    // Resolve, skipping missing IDs.
    pub fn resolve_existing(&self, ids: &[String]) -> Vec<&Message> {
        ids.iter().filter_map(|id| self.messages.get(id.as_str())).collect()
    }
}
