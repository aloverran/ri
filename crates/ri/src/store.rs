//! Session storage: the pool, sessions, and persistence.
//!
//! This module owns the in-memory object store (Pool) and its persistence
//! layer (Store). The core data types (Message, Context) live in `model`
//! -- this module handles filing them to disk and looking them up.
//!
//! On disk, each session is an append-only JSONL file with five line types:
//!
//! - Session header: `{"session": "name", "ts": "...", ...}`
//! - Message: `{"msg": "m1", "role": "user", "content": [...]}`
//! - Context: `{"context": "c1", "messages": ["m1", "m2"], "parents": [], "meta": {...}}`
//! - Head update: `{"head": "c1"}`
//! - Title update: `{"title": "Fix login crash"}`
//!
//! The session's current state is the last `{"head": ...}` line.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use chrono::Utc;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::model::{ContentBlock, Context, ContextId, Message, MessageId, Role, SessionId};

// -- Pool --

/// The shared object store. Messages and contexts live here, referenced by ID.
///
/// The pool doesn't know about sessions or files. It's a bag of objects
/// with lookup by ID. The Store layer populates it from disk and writes
/// new objects to session files.
pub struct Pool {
    messages: HashMap<MessageId, Message>,
    contexts: HashMap<ContextId, Context>,
}

impl Pool {
    pub fn new() -> Self {
        Pool {
            messages: HashMap::new(),
            contexts: HashMap::new(),
        }
    }

    // -- Messages (read) --

    pub fn get_message(&self, id: &str) -> Option<&Message> {
        self.messages.get(id)
    }

    /// Resolve an ordered list of message IDs to their messages.
    /// Silently skips IDs not found in the pool.
    pub fn resolve(&self, ids: &[MessageId]) -> Vec<&Message> {
        ids.iter().filter_map(|id| self.messages.get(id)).collect()
    }

    /// Resolve a context to its messages.
    pub fn resolve_context(&self, ctx: &Context) -> Vec<&Message> {
        self.resolve(&ctx.messages)
    }

    // -- Messages (write) --

    pub fn put_message(&mut self, msg: Message) {
        assert!(!msg.id.as_str().is_empty(), "message ID must not be empty (role={:?})", msg.role);
        self.messages.insert(msg.id.clone(), msg);
    }

    pub fn message_count(&self) -> usize {
        self.messages.len()
    }

    // -- Contexts (read) --

    pub fn get_context(&self, id: &str) -> Option<&Context> {
        self.contexts.get(id)
    }

    // -- Contexts (write) --

    pub fn put_context(&mut self, ctx: Context) {
        assert!(!ctx.id.as_str().is_empty(), "context ID must not be empty");
        self.contexts.insert(ctx.id.clone(), ctx);
    }

    pub fn context_count(&self) -> usize {
        self.contexts.len()
    }

    /// Find all contexts whose parents include the given ID (forward traversal).
    /// O(n) scan over all contexts in the pool.
    pub fn children(&self, id: &str) -> Vec<&Context> {
        self.contexts.values()
            .filter(|ctx| ctx.parents.iter().any(|p| p.as_str() == id))
            .collect()
    }
}

impl Default for Pool {
    fn default() -> Self {
        Self::new()
    }
}

// -- Session (the pointer) --

/// A named pointer to a context. Like a git branch.
///
/// Not stored in the pool -- it's the entry point that references into it.
/// On disk, the header is the first line and head updates are appended as
/// `{"head": "..."}` lines.
#[derive(Debug, Clone)]
pub struct Session {
    /// Human-readable name (e.g. "fix-login").
    pub name: String,
    /// File-stem ID (e.g. "2026-02-28_120000_fix-login"). Locates the file.
    pub file_id: SessionId,
    /// Current context this session points to.
    pub head: ContextId,
    pub cwd: Option<String>,
    /// File-stem ID of the parent session, if spawned by another.
    pub parent: Option<SessionId>,
    pub ts: String,
}

// -- On-disk line formats --

/// Session header line (first line of the JSONL file).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionHeader {
    pub session: String,
    pub ts: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
}

/// A message line in the JSONL file.
/// Uses "msg" instead of "id" to distinguish from other line types.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct MessageLine {
    msg: MessageId,
    role: Role,
    content: Vec<ContentBlock>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    meta: Option<serde_json::Value>,
}

/// A context line in the JSONL file. Serialized with `"context"` as the
/// ID key and `"messages"` for the message list.
///
/// Old files used `{"step": "...", "context": [...]}`. Those are
/// handled by the loader's dispatch code since the field name collision
/// prevents simple serde aliases.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ContextLine {
    context: ContextId,
    messages: Vec<MessageId>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    parents: Vec<ContextId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    meta: Option<serde_json::Value>,
}

/// A head-update line. The last one in the file wins.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct HeadLine {
    head: ContextId,
}

/// A title-update line. The last one in the file wins. Written by
/// background title generation -- separate from the session header
/// so titles can evolve without rewriting the file.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct TitleLine {
    title: String,
}

// -- Store --

/// Manages the pool and session files. Loads history from existing JSONL
/// files and writes new messages, contexts, and head updates.
pub struct Store {
    pub pool: Pool,
    sessions_dir: PathBuf,
    /// Loaded session metadata, keyed by file_id.
    sessions: HashMap<SessionId, Session>,
}

impl Store {
    pub fn new(sessions_dir: PathBuf) -> Self {
        Store {
            pool: Pool::new(),
            sessions_dir,
            sessions: HashMap::new(),
        }
    }

    pub fn default_dir() -> eyre::Result<PathBuf> {
        let home = dirs::home_dir()
            .ok_or_else(|| eyre::eyre!("Could not determine home directory"))?;
        Ok(home.join(".ri").join("sessions"))
    }

    pub fn get_session(&self, file_id: &str) -> Option<&Session> {
        self.sessions.get(file_id)
    }

    // -- Loading --

    /// Load all .jsonl session files into the pool and session map.
    pub fn load_all(&mut self) -> eyre::Result<()> {
        if !self.sessions_dir.exists() {
            return Ok(());
        }

        let mut entries: Vec<_> = fs::read_dir(&self.sessions_dir)?
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "jsonl"))
            .collect();
        entries.sort_by(|a, b| a.file_name().cmp(&b.file_name()));

        let file_count = entries.len();
        for entry in entries {
            if let Err(e) = self.load_file(&entry.path()) {
                tracing::warn!("Failed to load session file {}: {}", entry.path().display(), e);
            }
        }

        tracing::info!(
            "Loaded session history ({} files, {} messages, {} contexts)",
            file_count, self.pool.message_count(), self.pool.context_count(),
        );

        Ok(())
    }

    fn load_file(&mut self, path: &Path) -> eyre::Result<()> {
        let file_id = SessionId::new(
            path.file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("")
        );

        let file = File::open(path)?;
        let reader = BufReader::new(file);
        let mut header: Option<SessionHeader> = None;
        let mut head: Option<ContextId> = None;

        for (line_num, line) in reader.lines().enumerate() {
            let line = match line {
                Ok(l) => l,
                Err(e) => {
                    tracing::warn!("{}:{}: read error: {}", path.display(), line_num + 1, e);
                    break;
                }
            };

            let trimmed = line.trim();
            if trimmed.is_empty() { continue; }

            // Parse as generic JSON first, then dispatch by key.
            let obj: serde_json::Value = match serde_json::from_str(trimmed) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!("{}:{}: malformed JSON, skipping: {}", path.display(), line_num + 1, e);
                    continue;
                }
            };

            if obj.get("session").is_some() {
                if let Ok(h) = serde_json::from_value::<SessionHeader>(obj) {
                    header = Some(h);
                }
            } else if obj.get("msg").is_some() {
                if let Ok(ml) = serde_json::from_value::<MessageLine>(obj) {
                    self.pool.put_message(Message {
                        id: ml.msg,
                        role: ml.role,
                        content: ml.content,
                        meta: ml.meta,
                    });
                }
            } else if obj.get("context").is_some() || obj.get("step").is_some() {
                // Accept both new "context" lines and old "step" lines.
                // Old format: {"step":"s1", "context":["m1","m2"], ...}
                // New format: {"context":"c1", "messages":["m1","m2"], ...}
                let parsed = if obj.get("step").is_some() {
                    parse_legacy_step(&obj)
                } else {
                    serde_json::from_value::<ContextLine>(obj).ok()
                };
                if let Some(cl) = parsed {
                    self.pool.put_context(Context {
                        id: cl.context,
                        messages: cl.messages,
                        parents: cl.parents,
                        meta: cl.meta,
                    });
                }
            } else if obj.get("head").is_some() {
                if let Ok(hl) = serde_json::from_value::<HeadLine>(obj) {
                    head = Some(hl.head);
                }
            } else if obj.get("title").is_some() {
                if let Ok(tl) = serde_json::from_value::<TitleLine>(obj) {
                    if let Some(h) = header.as_mut() {
                        h.session = tl.title;
                    }
                }
            } else {
                tracing::warn!("{}:{}: unrecognized line type, skipping", path.display(), line_num + 1);
            }
        }

        if let (Some(h), Some(head_id)) = (header, head) {
            self.sessions.insert(file_id.clone(), Session {
                name: h.session,
                file_id,
                head: head_id,
                cwd: h.cwd,
                parent: h.parent.map(SessionId::new),
                ts: h.ts,
            });
        }

        Ok(())
    }

    // -- Writing --

    /// Create a new session file with an initial root context (empty).
    /// Returns the SessionId (timestamp-based file stem).
    pub fn create_session(
        &mut self,
        name: &str,
        cwd: &str,
        parent: Option<&SessionId>,
    ) -> eyre::Result<SessionId> {
        fs::create_dir_all(&self.sessions_dir)?;

        let now = Utc::now();
        let ts = now.to_rfc3339();
        let file_ts = now.format("%Y-%m-%d_%H%M%S");
        let slug = slugify(name);
        let filename = format!("{}_{}.jsonl", file_ts, slug);
        let path = self.sessions_dir.join(&filename);

        let file_id = SessionId::new(
            path.file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("")
        );

        let header = SessionHeader {
            session: name.to_string(),
            ts: ts.clone(),
            cwd: Some(cwd.to_string()),
            parent: parent.map(|p| p.to_string()),
        };

        // Write header, root context, and head pointer.
        let root_id = ContextId::new(scan_next_id(&path, "context"));
        let root_ctx = ContextLine {
            context: root_id.clone(),
            messages: Vec::new(),
            parents: Vec::new(),
            meta: None,
        };
        let head_line = HeadLine { head: root_id.clone() };

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        writeln!(file, "{}", serde_json::to_string(&header)?)?;
        writeln!(file, "{}", serde_json::to_string(&root_ctx)?)?;
        writeln!(file, "{}", serde_json::to_string(&head_line)?)?;

        // Register in pool and session map.
        self.pool.put_context(Context {
            id: root_id.clone(),
            messages: Vec::new(),
            parents: Vec::new(),
            meta: None,
        });

        let session = Session {
            name: name.to_string(),
            file_id: file_id.clone(),
            head: root_id,
            cwd: Some(cwd.to_string()),
            parent: parent.cloned(),
            ts,
        };
        self.sessions.insert(file_id.clone(), session);

        tracing::info!("Created session [{}] -> [{}]", name, file_id);
        Ok(file_id)
    }

    /// Write a message to a session file and add it to the pool.
    /// The store assigns a sequential ID automatically.
    pub fn write_message(
        &mut self,
        session_id: &SessionId,
        role: Role,
        content: Vec<ContentBlock>,
        meta: Option<serde_json::Value>,
    ) -> eyre::Result<Message> {
        let path = self.sessions_dir.join(format!("{}.jsonl", session_id));
        let id = MessageId::new(scan_next_id(&path, "msg"));

        let msg = Message { id, role, content, meta };

        let line = MessageLine {
            msg: msg.id.clone(),
            role: msg.role,
            content: msg.content.clone(),
            meta: msg.meta.clone(),
        };

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        writeln!(file, "{}", serde_json::to_string(&line)?)?;
        file.flush()?;

        tracing::debug!("Wrote {:?} message [{}] to session [{}]", msg.role, msg.id, session_id);
        self.pool.put_message(msg.clone());
        Ok(msg)
    }

    /// Create a new context from the current message list and update the
    /// session's head.
    ///
    /// After any meaningful change to the message list, call checkpoint
    /// to persist a new context. On reload, the head context's messages
    /// gives back exactly this list.
    pub fn checkpoint(
        &mut self,
        session_id: &SessionId,
        message_ids: &[MessageId],
        meta: Option<serde_json::Value>,
    ) -> eyre::Result<Context> {
        let parents = self.sessions.get(session_id.as_str())
            .map(|s| s.head.clone())
            .into_iter()
            .collect();
        let ctx = self.write_context(session_id, message_ids.to_vec(), parents, meta)?;
        self.update_head(session_id, &ctx.id)?;
        Ok(ctx)
    }

    /// Convenience: get the current context for a session (from its head).
    pub fn head_context(&self, session_id: &str) -> Option<&Context> {
        let session = self.sessions.get(session_id)?;
        self.pool.get_context(session.head.as_str())
    }

    /// Persist a generated title to the session file and update the in-memory
    /// session name. Append-only: writes a `{"title": "..."}` line.
    pub fn write_title(
        &mut self,
        session_id: &SessionId,
        title: &str,
    ) -> eyre::Result<()> {
        let path = self.sessions_dir.join(format!("{}.jsonl", session_id));
        let line = TitleLine { title: title.to_string() };
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        writeln!(file, "{}", serde_json::to_string(&line)?)?;
        file.flush()?;

        if let Some(session) = self.sessions.get_mut(session_id.as_str()) {
            session.name = title.to_string();
        }

        tracing::debug!("Wrote title [{}] to session [{}]", title, session_id);
        Ok(())
    }

    // -- Writing (contexts and head pointer) --

    /// Write a context to a session file and add it to the pool.
    ///
    /// Does NOT update the session's head pointer. Call `update_head`
    /// separately, or use `checkpoint` for the common
    /// write-context-and-advance-head pattern.
    pub fn write_context(
        &mut self,
        session_id: &SessionId,
        messages: Vec<MessageId>,
        parents: Vec<ContextId>,
        meta: Option<serde_json::Value>,
    ) -> eyre::Result<Context> {
        let path = self.sessions_dir.join(format!("{}.jsonl", session_id));
        let id = ContextId::new(scan_next_id(&path, "context"));

        let ctx = Context { id, messages, parents, meta };

        let ctx_line = ContextLine {
            context: ctx.id.clone(),
            messages: ctx.messages.clone(),
            parents: ctx.parents.clone(),
            meta: ctx.meta.clone(),
        };

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        writeln!(file, "{}", serde_json::to_string(&ctx_line)?)?;
        file.flush()?;

        tracing::debug!("Wrote context [{}] to session [{}] ({} messages, {} parents)",
            ctx.id, session_id, ctx.messages.len(), ctx.parents.len());

        self.pool.put_context(ctx.clone());
        Ok(ctx)
    }

    /// Update a session's head to point at the given context.
    /// Writes a `{"head": ...}` line to the session file and updates
    /// the in-memory session record.
    pub fn update_head(
        &mut self,
        session_id: &SessionId,
        context_id: &ContextId,
    ) -> eyre::Result<()> {
        let path = self.sessions_dir.join(format!("{}.jsonl", session_id));
        let head_line = HeadLine { head: context_id.clone() };

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        writeln!(file, "{}", serde_json::to_string(&head_line)?)?;
        file.flush()?;

        if let Some(session) = self.sessions.get_mut(session_id.as_str()) {
            session.head = context_id.clone();
        }

        tracing::debug!("Updated head of session [{}] to context [{}]", session_id, context_id);
        Ok(())
    }
}

// -- ID scanning --

/// Scan a session file for IDs matching `{prefix}_{N}` pattern and return
/// the next available ID. For context IDs, also scans for old "step" keys
/// to maintain uniqueness when appending to pre-migration files.
fn scan_next_id(path: &Path, kind: &str) -> String {
    let needle = format!("\"{}\":\"", kind);
    // Also scan the legacy key when looking for context IDs.
    let legacy_needle = if kind == "context" {
        Some("\"step\":\"".to_string())
    } else {
        None
    };

    let mut max_counter: u64 = 0;
    let mut prefix: Option<String> = None;

    if let Ok(file) = File::open(path) {
        let reader = BufReader::new(file);
        for line in reader.lines().flatten() {
            let trimmed = line.trim();
            if trimmed.is_empty() { continue; }

            // Try the primary needle, then the legacy needle.
            let id = extract_field_value(trimmed, &needle)
                .or_else(|| legacy_needle.as_ref().and_then(|ln| extract_field_value(trimmed, ln)));
            if let Some(id) = id {
                if let Some(pos) = id.rfind('_') {
                    if let Ok(n) = id[pos + 1..].parse::<u64>() {
                        prefix = Some(id[..pos].to_string());
                        max_counter = max_counter.max(n);
                    }
                }
            }
        }
    }

    let prefix = prefix.unwrap_or_else(|| {
        let stem = path.file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("s");
        gen_session_prefix(stem)
    });

    format!("{}_{}", prefix, max_counter + 1)
}

/// Fast field extraction without full JSON parse.
fn extract_field_value<'a>(line: &'a str, needle: &str) -> Option<&'a str> {
    let start = line.find(needle)? + needle.len();
    let end = start + line[start..].find('"')?;
    Some(&line[start..end])
}

/// Generate a session prefix from name + random suffix, used to create
/// human-readable but unique IDs within a session file.
fn gen_session_prefix(name: &str) -> String {
    let slug: String = name.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(6)
        .collect();
    let rand = &Uuid::new_v4().simple().to_string()[..6];
    if slug.is_empty() {
        format!("s_{}", rand)
    } else {
        format!("{}_{}", slug, rand)
    }
}

/// Parse a legacy `{"step": ..., "context": [...]}` line into a ContextLine.
fn parse_legacy_step(obj: &serde_json::Value) -> Option<ContextLine> {
    let id = obj.get("step")?.as_str()?;
    let messages: Vec<MessageId> = obj.get("context")?
        .as_array()?
        .iter()
        .filter_map(|v| v.as_str().map(MessageId::from))
        .collect();
    let parents: Vec<ContextId> = obj.get("parents")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str().map(ContextId::from)).collect())
        .unwrap_or_default();
    let meta = obj.get("meta").and_then(|v| {
        if v.is_null() { None } else { Some(v.clone()) }
    });
    Some(ContextLine {
        context: ContextId::from(id),
        messages,
        parents,
        meta,
    })
}

fn slugify(name: &str) -> String {
    let slug: String = name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let mut result = String::new();
    let mut prev_dash = false;
    for c in slug.chars() {
        if c == '-' {
            if !prev_dash && !result.is_empty() {
                result.push(c);
            }
            prev_dash = true;
        } else {
            result.push(c);
            prev_dash = false;
        }
    }
    result.trim_end_matches('-').to_string()
}
