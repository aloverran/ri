// Session management -- tree-structured JSONL persistence.
//
// Each session is a JSONL file where every entry has an id and parentId,
// forming a tree. This enables branching, forking, and non-linear navigation.
//
// File format: one JSON object per line, append-only.
// Location: ~/.ri/sessions/{timestamp}-{short_id}.jsonl

use chrono::Utc;
use ri::types::Message;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use uuid::Uuid;

pub const SESSION_VERSION: u32 = 3;

// -- Entry types --

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionEntry {
    Session {
        id: String,
        version: u32,
        timestamp: String,
        cwd: String,
    },
    Message {
        id: String,
        #[serde(rename = "parentId")]
        parent_id: String,
        timestamp: String,
        message: Message,
    },
    ModelChange {
        id: String,
        #[serde(rename = "parentId")]
        parent_id: String,
        provider: String,
        #[serde(rename = "modelId")]
        model_id: String,
    },
    Compaction {
        id: String,
        #[serde(rename = "parentId")]
        parent_id: String,
        summary: String,
        #[serde(rename = "firstKeptEntryId")]
        first_kept_entry_id: String,
    },
    ThinkingLevelChange {
        id: String,
        #[serde(rename = "parentId")]
        parent_id: String,
        level: String,
    },
}

impl SessionEntry {
    pub fn id(&self) -> &str {
        match self {
            SessionEntry::Session { id, .. }
            | SessionEntry::Message { id, .. }
            | SessionEntry::ModelChange { id, .. }
            | SessionEntry::Compaction { id, .. }
            | SessionEntry::ThinkingLevelChange { id, .. } => id,
        }
    }

    pub fn parent_id(&self) -> Option<&str> {
        match self {
            SessionEntry::Session { .. } => None,
            SessionEntry::Message { parent_id, .. }
            | SessionEntry::ModelChange { parent_id, .. }
            | SessionEntry::Compaction { parent_id, .. }
            | SessionEntry::ThinkingLevelChange { parent_id, .. } => Some(parent_id),
        }
    }

    pub fn is_session_header(&self) -> bool {
        matches!(self, SessionEntry::Session { .. })
    }
}

// -- Tree node for navigation --

#[derive(Debug, Clone)]
pub struct SessionTreeNode {
    pub entry_id: String,
    pub children: Vec<SessionTreeNode>,
}

// -- Session metadata for listing --

#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub path: PathBuf,
    pub id: String,
    pub timestamp: String,
    pub cwd: String,
}

// -- Backing store abstraction --

enum Backing {
    File(File),
    Memory,
}

// -- SessionManager --

pub struct SessionManager {
    entries: Vec<SessionEntry>,
    /// id -> index in entries vec
    index: HashMap<String, usize>,
    backing: Backing,
    path: Option<PathBuf>,
    current_leaf_id: Option<String>,
}

impl SessionManager {
    // -- Constructors --

    /// Create a new file-backed session. Writes session header immediately.
    pub fn create(cwd: &str) -> eyre::Result<Self> {
        let sessions_dir = sessions_dir()?;
        fs::create_dir_all(&sessions_dir)?;

        let session_id = new_id();
        let now = Utc::now();
        let timestamp = now.to_rfc3339();
        let short_id = &session_id[..8];
        let file_ts = now.format("%Y%m%d-%H%M%S");
        let filename = format!("{}-{}.jsonl", file_ts, short_id);
        let path = sessions_dir.join(filename);

        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;

        let mut mgr = SessionManager {
            entries: Vec::new(),
            index: HashMap::new(),
            backing: Backing::File(file),
            path: Some(path),
            current_leaf_id: None,
        };

        let header = SessionEntry::Session {
            id: session_id,
            version: SESSION_VERSION,
            timestamp,
            cwd: cwd.to_string(),
        };
        mgr.append(header)?;

        Ok(mgr)
    }

    /// Open an existing session file, loading all entries.
    pub fn open(path: &Path) -> eyre::Result<Self> {
        let reader = BufReader::new(File::open(path)?);
        let mut entries = Vec::new();
        let mut index = HashMap::new();

        for line in reader.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let entry: SessionEntry = serde_json::from_str(&line)?;
            let id = entry.id().to_string();
            index.insert(id, entries.len());
            entries.push(entry);
        }

        // Find the leaf: the entry with no children (last appended on the longest chain).
        let leaf_id = find_leaf(&entries);

        let file = OpenOptions::new().append(true).open(path)?;

        Ok(SessionManager {
            entries,
            index,
            backing: Backing::File(file),
            path: Some(path.to_path_buf()),
            current_leaf_id: leaf_id,
        })
    }

    /// In-memory session for testing. No file I/O.
    pub fn in_memory() -> Self {
        SessionManager {
            entries: Vec::new(),
            index: HashMap::new(),
            backing: Backing::Memory,
            path: None,
            current_leaf_id: None,
        }
    }

    // -- Write operations --

    /// Append a raw entry. Writes to JSONL file if file-backed.
    pub fn append(&mut self, entry: SessionEntry) -> eyre::Result<()> {
        if let Backing::File(ref mut file) = self.backing {
            let line = serde_json::to_string(&entry)?;
            writeln!(file, "{}", line)?;
        }

        let id = entry.id().to_string();
        self.current_leaf_id = Some(id.clone());
        self.index.insert(id, self.entries.len());
        self.entries.push(entry);
        Ok(())
    }

    /// Append a user or assistant message. Returns the entry id.
    pub fn append_message(&mut self, message: Message) -> eyre::Result<String> {
        let id = new_id();
        let parent_id = self
            .current_leaf_id
            .clone()
            .unwrap_or_default();

        let entry = SessionEntry::Message {
            id: id.clone(),
            parent_id,
            timestamp: Utc::now().to_rfc3339(),
            message,
        };
        self.append(entry)?;
        Ok(id)
    }

    /// Record a model change.
    pub fn append_model_change(&mut self, provider: &str, model_id: &str) -> eyre::Result<String> {
        let id = new_id();
        let parent_id = self.current_leaf_id.clone().unwrap_or_default();

        let entry = SessionEntry::ModelChange {
            id: id.clone(),
            parent_id,
            provider: provider.to_string(),
            model_id: model_id.to_string(),
        };
        self.append(entry)?;
        Ok(id)
    }

    /// Record a compaction.
    pub fn append_compaction(
        &mut self,
        summary: String,
        first_kept_entry_id: String,
    ) -> eyre::Result<String> {
        let id = new_id();
        let parent_id = self.current_leaf_id.clone().unwrap_or_default();

        let entry = SessionEntry::Compaction {
            id: id.clone(),
            parent_id,
            summary,
            first_kept_entry_id,
        };
        self.append(entry)?;
        Ok(id)
    }

    /// Record a thinking level change.
    pub fn append_thinking_level_change(&mut self, level: &str) -> eyre::Result<String> {
        let id = new_id();
        let parent_id = self.current_leaf_id.clone().unwrap_or_default();

        let entry = SessionEntry::ThinkingLevelChange {
            id: id.clone(),
            parent_id,
            level: level.to_string(),
        };
        self.append(entry)?;
        Ok(id)
    }

    // -- Read operations --

    /// Get all entries.
    pub fn entries(&self) -> &[SessionEntry] {
        &self.entries
    }

    /// Get a single entry by id.
    pub fn get_entry(&self, id: &str) -> Option<&SessionEntry> {
        self.index.get(id).map(|&i| &self.entries[i])
    }

    /// Current leaf id (the "cursor" position in the tree).
    pub fn current_leaf_id(&self) -> Option<&str> {
        self.current_leaf_id.as_deref()
    }

    /// Walk the parentId chain from leaf to root. Returns entries in root-to-leaf order.
    pub fn get_branch(&self, from_id: Option<&str>) -> Vec<&SessionEntry> {
        let leaf = from_id
            .or(self.current_leaf_id.as_deref());

        let leaf = match leaf {
            Some(id) => id,
            None => return Vec::new(),
        };

        let mut chain = Vec::new();
        let mut current = Some(leaf);

        while let Some(id) = current {
            if let Some(&idx) = self.index.get(id) {
                let entry = &self.entries[idx];
                chain.push(entry);
                current = entry.parent_id();
            } else {
                break;
            }
        }

        chain.reverse();
        chain
    }

    /// Build a tree structure for the full session.
    pub fn get_tree(&self) -> Option<SessionTreeNode> {
        // Find root (session header).
        let root = self.entries.iter().find(|e| e.is_session_header())?;
        let root_id = root.id().to_string();

        // Build children map: parent_id -> vec of child ids.
        let mut children_map: HashMap<&str, Vec<&str>> = HashMap::new();
        for entry in &self.entries {
            if let Some(pid) = entry.parent_id() {
                children_map.entry(pid).or_default().push(entry.id());
            }
        }

        Some(build_tree_node(&root_id, &children_map))
    }

    // -- Navigation --

    /// Switch to a different leaf (for branch navigation).
    pub fn switch_to_leaf(&mut self, entry_id: &str) {
        if self.index.contains_key(entry_id) {
            self.current_leaf_id = Some(entry_id.to_string());
        }
    }

    /// Fork from a given entry, creating a new branch point.
    /// Sets the current leaf to the fork point so the next append
    /// branches from there.
    pub fn fork(&mut self, from_entry_id: &str) -> bool {
        if self.index.contains_key(from_entry_id) {
            self.current_leaf_id = Some(from_entry_id.to_string());
            true
        } else {
            false
        }
    }

    /// Get the file path if file-backed.
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    // -- Session discovery --

    /// List all sessions in ~/.ri/sessions/, most recent first.
    pub fn list_sessions() -> eyre::Result<Vec<SessionInfo>> {
        let dir = sessions_dir()?;
        if !dir.exists() {
            return Ok(Vec::new());
        }

        let mut sessions = Vec::new();
        let mut dir_entries: Vec<_> = fs::read_dir(&dir)?
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.path()
                    .extension()
                    .map_or(false, |ext| ext == "jsonl")
            })
            .collect();

        // Sort by filename descending (newest first, since filenames start with timestamp).
        dir_entries.sort_by(|a, b| b.file_name().cmp(&a.file_name()));

        for dir_entry in dir_entries {
            let path = dir_entry.path();
            if let Ok(info) = read_session_header(&path) {
                sessions.push(info);
            }
        }

        Ok(sessions)
    }
}

// -- Helpers --

fn new_id() -> String {
    Uuid::new_v4().to_string()
}

fn sessions_dir() -> eyre::Result<PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| eyre::eyre!("Could not determine home directory"))?;
    Ok(home.join(".ri").join("sessions"))
}

/// Read just the session header from a JSONL file.
fn read_session_header(path: &Path) -> eyre::Result<SessionInfo> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);

    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let entry: SessionEntry = serde_json::from_str(&line)?;
        if let SessionEntry::Session {
            id,
            timestamp,
            cwd,
            ..
        } = entry
        {
            return Ok(SessionInfo {
                path: path.to_path_buf(),
                id,
                timestamp,
                cwd,
            });
        }
        break;
    }

    Err(eyre::eyre!(
        "No session header found in {}",
        path.display()
    ))
}

/// Find the leaf entry: the entry whose id is not any other entry's parentId.
/// Among multiple leaves, pick the one appended last (highest index).
fn find_leaf(entries: &[SessionEntry]) -> Option<String> {
    if entries.is_empty() {
        return None;
    }

    // Collect all ids that appear as a parentId.
    let mut is_parent = std::collections::HashSet::new();
    for entry in entries {
        if let Some(pid) = entry.parent_id() {
            is_parent.insert(pid);
        }
    }

    // Leaf = id not in is_parent set. Take the last one by position.
    entries
        .iter()
        .rev()
        .find(|e| !is_parent.contains(e.id()))
        .map(|e| e.id().to_string())
}

fn build_tree_node(
    id: &str,
    children_map: &HashMap<&str, Vec<&str>>,
) -> SessionTreeNode {
    let children = children_map
        .get(id)
        .map(|kids| {
            kids.iter()
                .map(|kid| build_tree_node(kid, children_map))
                .collect()
        })
        .unwrap_or_default();

    SessionTreeNode {
        entry_id: id.to_string(),
        children,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_in_memory_append_and_branch() {
        let mut mgr = SessionManager::in_memory();

        // Append session header.
        let header = SessionEntry::Session {
            id: "root".to_string(),
            version: SESSION_VERSION,
            timestamp: "2025-01-01T00:00:00Z".to_string(),
            cwd: "/tmp".to_string(),
        };
        mgr.append(header).unwrap();

        // Append messages.
        let id1 = mgr
            .append_message(Message::user("Hello"))
            .unwrap();
        let id2 = mgr
            .append_message(Message::assistant("Hi there"))
            .unwrap();

        assert_eq!(mgr.entries().len(), 3);
        assert_eq!(mgr.current_leaf_id(), Some(id2.as_str()));

        // Branch should walk root -> msg1 -> msg2.
        let branch = mgr.get_branch(None);
        assert_eq!(branch.len(), 3);
        assert_eq!(branch[0].id(), "root");
        assert_eq!(branch[1].id(), id1);
        assert_eq!(branch[2].id(), id2);
    }

    #[test]
    fn test_fork_creates_branch() {
        let mut mgr = SessionManager::in_memory();

        let header = SessionEntry::Session {
            id: "root".to_string(),
            version: SESSION_VERSION,
            timestamp: "2025-01-01T00:00:00Z".to_string(),
            cwd: "/tmp".to_string(),
        };
        mgr.append(header).unwrap();

        let id1 = mgr.append_message(Message::user("First")).unwrap();
        let _id2 = mgr
            .append_message(Message::assistant("Second"))
            .unwrap();

        // Fork from id1 -- next append branches from there.
        assert!(mgr.fork(&id1));
        let id3 = mgr
            .append_message(Message::user("Alternate"))
            .unwrap();

        // Branch from id3 should be root -> id1 -> id3 (not id2).
        let branch = mgr.get_branch(Some(&id3));
        assert_eq!(branch.len(), 3);
        assert_eq!(branch[0].id(), "root");
        assert_eq!(branch[1].id(), id1);
        assert_eq!(branch[2].id(), id3);

        // Tree should show the fork.
        let tree = mgr.get_tree().unwrap();
        assert_eq!(tree.entry_id, "root");
        assert_eq!(tree.children.len(), 1); // root -> id1
        assert_eq!(tree.children[0].children.len(), 2); // id1 -> id2, id3
    }

    #[test]
    fn test_get_branch_from_specific_leaf() {
        let mut mgr = SessionManager::in_memory();

        let header = SessionEntry::Session {
            id: "root".to_string(),
            version: SESSION_VERSION,
            timestamp: "2025-01-01T00:00:00Z".to_string(),
            cwd: "/tmp".to_string(),
        };
        mgr.append(header).unwrap();

        let id1 = mgr.append_message(Message::user("A")).unwrap();
        let id2 = mgr.append_message(Message::assistant("B")).unwrap();

        // Get branch from id1 (not the current leaf).
        let branch = mgr.get_branch(Some(&id1));
        assert_eq!(branch.len(), 2);
        assert_eq!(branch[0].id(), "root");
        assert_eq!(branch[1].id(), id1);

        // Current leaf should still be id2.
        let branch = mgr.get_branch(None);
        assert_eq!(branch.len(), 3);
        assert_eq!(branch[2].id(), id2);
    }

    #[test]
    fn test_model_change_and_compaction() {
        let mut mgr = SessionManager::in_memory();

        let header = SessionEntry::Session {
            id: "root".to_string(),
            version: SESSION_VERSION,
            timestamp: "2025-01-01T00:00:00Z".to_string(),
            cwd: "/tmp".to_string(),
        };
        mgr.append(header).unwrap();

        let _msg_id = mgr.append_message(Message::user("Hello")).unwrap();

        let mc_id = mgr
            .append_model_change("anthropic", "claude-opus-4")
            .unwrap();
        assert!(mgr.get_entry(&mc_id).is_some());

        let comp_id = mgr
            .append_compaction(
                "Summary of conversation".to_string(),
                "root".to_string(),
            )
            .unwrap();
        assert!(mgr.get_entry(&comp_id).is_some());

        // Branch includes all entries.
        let branch = mgr.get_branch(None);
        assert_eq!(branch.len(), 4); // root, msg, model_change, compaction
    }

    #[test]
    fn test_entry_serialization_roundtrip() {
        let entry = SessionEntry::Message {
            id: "abc".to_string(),
            parent_id: "root".to_string(),
            timestamp: "2025-01-01T00:00:00Z".to_string(),
            message: Message::user("Test message"),
        };

        let json = serde_json::to_string(&entry).unwrap();
        let parsed: SessionEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.id(), "abc");
        assert_eq!(parsed.parent_id(), Some("root"));
    }

    #[test]
    fn test_empty_branch() {
        let mgr = SessionManager::in_memory();
        let branch = mgr.get_branch(None);
        assert!(branch.is_empty());
    }

    #[test]
    fn test_switch_to_leaf() {
        let mut mgr = SessionManager::in_memory();

        let header = SessionEntry::Session {
            id: "root".to_string(),
            version: SESSION_VERSION,
            timestamp: "2025-01-01T00:00:00Z".to_string(),
            cwd: "/tmp".to_string(),
        };
        mgr.append(header).unwrap();

        let id1 = mgr.append_message(Message::user("First")).unwrap();
        let id2 = mgr
            .append_message(Message::assistant("Second"))
            .unwrap();

        assert_eq!(mgr.current_leaf_id(), Some(id2.as_str()));

        mgr.switch_to_leaf(&id1);
        assert_eq!(mgr.current_leaf_id(), Some(id1.as_str()));

        // Branch from id1 should be root -> id1.
        let branch = mgr.get_branch(None);
        assert_eq!(branch.len(), 2);
    }
}
