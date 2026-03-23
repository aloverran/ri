//! Session storage: the pool, sessions, and persistence.
//!
//! This module owns the in-memory object store (Pool) and its persistence
//! layer (Store). The core data types (Message, Context) live in `model`
//! -- this module handles filing them to disk and looking them up.
//!
//! On disk, each file is an append-only JSONL store with three line types:
//!
//! - Message: `{"msg": "2603_a1b2c3d4e5f6", "role": "user", "content": [...]}`
//! - Context: `{"context": "2603_c1d2e3f4a5b6", "messages": [...], "parents": [...]}`
//! - Session: `{"session": "2026-03-08_120000_fix-login", "head": "...", "name": "...", ...}`
//!
//! A single file can contain multiple sessions. The last session line per
//! session ID wins. Files are named by convention after the session that
//! created them, but the naming is not load-bearing -- any valid JSONL
//! store file can be loaded from anywhere.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use chrono::Utc;
use serde::{Deserialize, Serialize};

use crate::model::{ContentBlock, Context, ContextId, Message, MessageId, Role, SessionId, gen_obj_id};

/// The shared object store. Messages, contexts, and sessions live here.
///
/// Three object types, one pool. This is the complete in-memory
/// representation of a store file (or a set of store files).
pub struct Pool {
    messages: HashMap<MessageId, Message>,
    contexts: HashMap<ContextId, Context>,
    sessions: HashMap<SessionId, Session>,
}

impl Pool {
    pub fn new() -> Self {
        Pool {
            messages: HashMap::new(),
            contexts: HashMap::new(),
            sessions: HashMap::new(),
        }
    }

    pub fn get_message(&self, id: &str) -> Option<&Message> {
        self.messages.get(id)
    }

    /// Resolve an ordered list of message IDs to their messages.
    /// Skips IDs not found in the pool, but warns so missing data is visible.
    pub fn resolve(&self, ids: &[MessageId]) -> Vec<&Message> {
        ids.iter().filter_map(|id| {
            let msg = self.messages.get(id);
            if msg.is_none() {
                tracing::warn!("Message [{}] not found during context resolution", id);
            }
            msg
        }).collect()
    }

    /// Resolve a context to its messages.
    pub fn resolve_context(&self, ctx: &Context) -> Vec<&Message> {
        self.resolve(&ctx.messages)
    }

    pub fn put_message(&mut self, msg: Message) {
        assert!(!msg.id.as_str().is_empty(), "message ID must not be empty (role={:?})", msg.role);
        self.messages.insert(msg.id.clone(), msg);
    }

    pub fn message_count(&self) -> usize {
        self.messages.len()
    }

    pub fn get_context(&self, id: &str) -> Option<&Context> {
        self.contexts.get(id)
    }

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

    pub fn get_session(&self, id: &str) -> Option<&Session> {
        self.sessions.get(id)
    }

    pub fn put_session(&mut self, session: Session) {
        self.sessions.insert(session.id.clone(), session);
    }

    pub fn session_count(&self) -> usize {
        self.sessions.len()
    }

    /// Iterate over all sessions in the pool.
    pub fn sessions(&self) -> impl Iterator<Item = &Session> {
        self.sessions.values()
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
/// Sessions live in the pool alongside messages and contexts. On disk,
/// each session is a full-snapshot line: `{"session": "id", "head": "ctx", ...}`.
/// The last line per session ID wins.
#[derive(Debug, Clone)]
pub struct Session {
    /// Human-readable display name (e.g. "Fix login crash").
    pub name: String,
    /// Unique identifier (e.g. "2026-02-28_120000_fix-login").
    pub id: SessionId,
    /// Current context this session points to.
    pub head: ContextId,
    pub cwd: Option<String>,
    /// ID of the parent session, if spawned by another.
    pub parent: Option<SessionId>,
    pub ts: String,
    /// File stem of the JSONL file this session writes to.
    /// For top-level sessions this equals `id`. For sub-agents
    /// this is the parent's file, so all related objects stay together.
    pub file: String,
}

// -- On-disk line formats --

/// A session pointer line in the JSONL file. Full-snapshot: every field
/// is written every time, last line per session ID wins.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SessionLine {
    session: SessionId,
    head: ContextId,
    name: String,
    ts: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    parent: Option<SessionId>,
}

/// A message line in the JSONL file.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct MessageLine {
    msg: MessageId,
    role: Role,
    content: Vec<ContentBlock>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    meta: Option<serde_json::Value>,
}

/// A context line in the JSONL file.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ContextLine {
    context: ContextId,
    messages: Vec<MessageId>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    parents: Vec<ContextId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    meta: Option<serde_json::Value>,
}

/// Manages the pool and JSONL files. Loads history from existing files
/// and writes new messages, contexts, and session pointer updates.
pub struct Store {
    pub pool: Pool,
    sessions_dir: PathBuf,
}

impl Store {
    pub fn new(sessions_dir: PathBuf) -> Self {
        Store {
            pool: Pool::new(),
            sessions_dir,
        }
    }

    pub fn default_dir() -> eyre::Result<PathBuf> {
        let home = dirs::home_dir()
            .ok_or_else(|| eyre::eyre!("Could not determine home directory"))?;
        Ok(home.join(".ri").join("sessions"))
    }

    pub fn get_session(&self, id: &str) -> Option<&Session> {
        self.pool.get_session(id)
    }

    /// Resolve which file a session writes to. Returns an error if the
    /// session isn't in the pool -- this catches misuse (writing before
    /// creating) instead of silently creating orphan files.
    fn resolve_file(&self, session_id: &SessionId) -> eyre::Result<String> {
        self.pool.get_session(session_id.as_str())
            .map(|s| s.file.clone())
            .ok_or_else(|| eyre::eyre!(
                "session '{}' not found in pool (write before create?)", session_id
            ))
    }

    fn file_path(&self, file_stem: &str) -> PathBuf {
        self.sessions_dir.join(format!("{}.jsonl", file_stem))
    }

    /// Load all .jsonl files into the pool.
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
                tracing::warn!("Failed to load store file {}: {}", entry.path().display(), e);
            }
        }

        tracing::info!(
            "Loaded store ({} files, {} messages, {} contexts, {} sessions)",
            file_count, self.pool.message_count(), self.pool.context_count(),
            self.pool.session_count(),
        );

        Ok(())
    }

    fn load_file(&mut self, path: &Path) -> eyre::Result<()> {
        let file_stem = path.file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();

        let file = File::open(path)?;
        let reader = BufReader::new(file);

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

            let obj: serde_json::Value = match serde_json::from_str(trimmed) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!("{}:{}: malformed JSON, skipping: {}", path.display(), line_num + 1, e);
                    continue;
                }
            };

            // Three line types: session (has both "session" + "head"), msg, context.
            if obj.get("session").is_some() && obj.get("head").is_some() {
                if let Ok(sl) = serde_json::from_value::<SessionLine>(obj) {
                    self.pool.put_session(Session {
                        name: sl.name,
                        id: sl.session,
                        head: sl.head,
                        cwd: sl.cwd,
                        parent: sl.parent,
                        ts: sl.ts,
                        file: file_stem.clone(),
                    });
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
            } else if obj.get("context").is_some() {
                if let Ok(cl) = serde_json::from_value::<ContextLine>(obj) {
                    self.pool.put_context(Context {
                        id: cl.context,
                        messages: cl.messages,
                        parents: cl.parents,
                        meta: cl.meta,
                    });
                }
            } else {
                tracing::warn!("{}:{}: unrecognized line format, skipping", path.display(), line_num + 1);
            }
        }

        Ok(())
    }

    /// Create a new session, optionally reusing an existing file.
    ///
    /// If `file` is `None`, creates a new JSONL file named after the session.
    /// If `file` is `Some(stem)`, writes to the existing file (for sub-agents
    /// sharing the parent's file).
    ///
    /// Either way, writes a root context and a session pointer line to the file.
    pub fn create_session(
        &mut self,
        name: &str,
        cwd: &str,
        parent: Option<&SessionId>,
        file: Option<&str>,
    ) -> eyre::Result<SessionId> {
        let now = Utc::now();
        let ts = now.to_rfc3339();

        // Determine file stem and session ID.
        let (file_stem, session_id) = if let Some(f) = file {
            // Sub-agent: reuse existing file, generate a unique session ID.
            let file_ts = now.format("%Y-%m-%d_%H%M%S");
            let slug = slugify(name);
            let rand = &uuid::Uuid::new_v4().simple().to_string()[..4];
            let sid = SessionId::new(format!("{}_{}_{}", file_ts, slug, rand));
            (f.to_string(), sid)
        } else {
            // Top-level: create a new file named after the session.
            fs::create_dir_all(&self.sessions_dir)?;
            let file_ts = now.format("%Y-%m-%d_%H%M%S");
            let slug = slugify(name);
            let stem = format!("{}_{}", file_ts, slug);
            (stem.clone(), SessionId::new(stem))
        };

        let path = self.file_path(&file_stem);

        // Write root context + session pointer.
        let root_id = ContextId::new(gen_obj_id());
        let root_ctx = ContextLine {
            context: root_id.clone(),
            messages: Vec::new(),
            parents: Vec::new(),
            meta: None,
        };
        let session_line = SessionLine {
            session: session_id.clone(),
            head: root_id.clone(),
            name: name.to_string(),
            ts: ts.clone(),
            cwd: Some(cwd.to_string()),
            parent: parent.cloned(),
        };

        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        writeln!(f, "{}", serde_json::to_string(&root_ctx)?)?;
        writeln!(f, "{}", serde_json::to_string(&session_line)?)?;
        f.flush()?;

        // Register in pool.
        self.pool.put_context(Context {
            id: root_id.clone(),
            messages: Vec::new(),
            parents: Vec::new(),
            meta: None,
        });
        self.pool.put_session(Session {
            name: name.to_string(),
            id: session_id.clone(),
            head: root_id,
            cwd: Some(cwd.to_string()),
            parent: parent.cloned(),
            ts,
            file: file_stem,
        });

        tracing::info!("Created session [{}] -> [{}]", name, session_id);
        Ok(session_id)
    }

    /// Write a message to the file associated with a session.
    pub fn write_message(
        &mut self,
        session_id: &SessionId,
        role: Role,
        content: Vec<ContentBlock>,
        meta: Option<serde_json::Value>,
    ) -> eyre::Result<Message> {
        let file_stem = self.resolve_file(session_id)?;
        let path = self.file_path(&file_stem);
        let id = MessageId::new(gen_obj_id());

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

        tracing::debug!("Wrote {:?} message [{}] to [{}]", msg.role, msg.id, file_stem);
        self.pool.put_message(msg.clone());
        Ok(msg)
    }

    /// Write a context to the file associated with a session.
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
        let file_stem = self.resolve_file(session_id)?;
        let path = self.file_path(&file_stem);
        let id = ContextId::new(gen_obj_id());

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

        tracing::debug!("Wrote context [{}] to [{}] ({} messages, {} parents)",
            ctx.id, file_stem, ctx.messages.len(), ctx.parents.len());
        self.pool.put_context(ctx.clone());
        Ok(ctx)
    }

    /// Create a new context and update the session's head.
    pub fn checkpoint(
        &mut self,
        session_id: &SessionId,
        message_ids: &[MessageId],
        meta: Option<serde_json::Value>,
    ) -> eyre::Result<Context> {
        let parents = self.pool.get_session(session_id.as_str())
            .map(|s| s.head.clone())
            .into_iter()
            .collect();
        let ctx = self.write_context(session_id, message_ids.to_vec(), parents, meta)?;
        self.update_head(session_id, &ctx.id)?;
        Ok(ctx)
    }

    /// Get the current context for a session (from its head).
    pub fn head_context(&self, session_id: &str) -> Option<&Context> {
        let session = self.pool.get_session(session_id)?;
        self.pool.get_context(session.head.as_str())
    }

    /// Update a session's head and write a full-snapshot session line.
    pub fn update_head(
        &mut self,
        session_id: &SessionId,
        context_id: &ContextId,
    ) -> eyre::Result<()> {
        let session = self.pool.sessions.get_mut(session_id.as_str())
            .ok_or_else(|| eyre::eyre!("session '{}' not found in pool", session_id))?;
        session.head = context_id.clone();
        let snapshot = session.clone();

        self.write_session_line(&snapshot)
    }

    /// Update a session's display name and persist it.
    pub fn write_title(
        &mut self,
        session_id: &SessionId,
        title: &str,
    ) -> eyre::Result<()> {
        let session = self.pool.sessions.get_mut(session_id.as_str())
            .ok_or_else(|| eyre::eyre!("session '{}' not found in pool", session_id))?;
        session.name = title.to_string();
        let snapshot = session.clone();

        self.write_session_line(&snapshot)
    }

    /// Append a full-snapshot session line to the session's file.
    fn write_session_line(&self, session: &Session) -> eyre::Result<()> {
        let path = self.file_path(&session.file);
        let line = SessionLine {
            session: session.id.clone(),
            head: session.head.clone(),
            name: session.name.clone(),
            ts: session.ts.clone(),
            cwd: session.cwd.clone(),
            parent: session.parent.clone(),
        };

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        writeln!(file, "{}", serde_json::to_string(&line)?)?;
        file.flush()?;

        tracing::debug!("Wrote session line [{}] to [{}]", session.id, session.file);
        Ok(())
    }
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

