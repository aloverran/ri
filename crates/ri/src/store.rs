//! Session storage: the pool, sessions, and persistence.
//!
//! `Store` is a shared database of messages, contexts, and sessions.
//! Consumers see a thread-safe store with persistence guarantees:
//! writes hit disk before becoming visible in memory.
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
//!
//! Designed for sharing via `Arc<Store>` across many concurrent tokio tasks.
//! Internal locking means all methods take `&self`.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use chrono::Utc;
use serde::{Deserialize, Serialize};

use crate::model::{ContentBlock, Context, ContextId, Message, MessageId, Role, SessionId, gen_obj_id};

/// The in-memory object pool. Messages, contexts, and sessions live here.
/// Not directly accessible -- all reads go through `Store` methods which
/// handle locking internally.
pub(crate) struct Pool {
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
    /// SSH target (e.g. "john@laptop.tailnet") for remote sessions.
    pub host: Option<String>,
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
    host: Option<String>,
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

/// A shared database of messages, contexts, and sessions.
///
/// Thread-safe: all methods take `&self`. Designed to be shared via `Arc<Store>`
/// across many concurrent tokio tasks. Internal `Mutex<Pool>` protects the
/// in-memory state; a separate write lock serializes file appends.
///
/// Persistence guarantee: writes hit disk (append + flush) before the
/// in-memory pool is updated. A crash at any point leaves the JSONL files
/// in a consistent prefix state.
///
/// Every public read method follows the same pattern: acquire the pool lock,
/// do work, release the lock, return owned data. The lock is never held
/// across caller-provided code (no closures, no returned guards). This
/// makes single-mutex deadlocks statically impossible.
pub struct Store {
    pool: Mutex<Pool>,
    sessions_dir: PathBuf,
    /// Serializes all JSONL file appends. Prevents interleaved lines when
    /// multiple sessions (including sub-agents sharing a parent file)
    /// write concurrently. Held only during serialize + write + flush.
    write_lock: Mutex<()>,
    /// File stems already loaded into the pool. Checked during `refresh()`
    /// to detect new files from external writers (e.g. ri-cli).
    loaded_files: Mutex<HashSet<String>>,
}

impl Store {
    /// Open the store, loading all existing JSONL files from disk.
    ///
    /// This is the only full disk scan. After this, all reads come from
    /// the in-memory pool, and writes use write-through semantics.
    pub fn open(sessions_dir: PathBuf) -> eyre::Result<Self> {
        let store = Store {
            pool: Mutex::new(Pool::new()),
            sessions_dir,
            write_lock: Mutex::new(()),
            loaded_files: Mutex::new(HashSet::new()),
        };
        store.load_all()?;
        Ok(store)
    }

    pub fn default_dir() -> eyre::Result<PathBuf> {
        let home = dirs::home_dir()
            .ok_or_else(|| eyre::eyre!("Could not determine home directory"))?;
        Ok(home.join(".ri").join("sessions"))
    }

    // -- Read methods (brief pool lock per call) --

    pub fn get_message(&self, id: &str) -> Option<Message> {
        self.pool.lock().unwrap().get_message(id).cloned()
    }

    pub fn get_context(&self, id: &str) -> Option<Context> {
        self.pool.lock().unwrap().get_context(id).cloned()
    }

    pub fn get_session(&self, id: &str) -> Option<Session> {
        self.pool.lock().unwrap().get_session(id).cloned()
    }

    /// Get the current head context for a session.
    pub fn head_context(&self, session_id: &str) -> Option<Context> {
        let pool = self.pool.lock().unwrap();
        let session = pool.get_session(session_id)?;
        pool.get_context(session.head.as_str()).cloned()
    }

    /// Resolve message IDs to cloned messages (for sending to LLM calls, etc.).
    pub fn resolve_cloned(&self, ids: &[MessageId]) -> Vec<Message> {
        let pool = self.pool.lock().unwrap();
        pool.resolve(ids).into_iter().cloned().collect()
    }

    /// List all sessions. Also ingests any new JSONL files from external
    /// writers (e.g. ri-cli) that appeared since the last check.
    pub fn sessions(&self) -> Vec<Session> {
        self.refresh_if_needed();
        self.pool.lock().unwrap().sessions().cloned().collect()
    }

    /// Find all contexts whose parents include the given ID.
    pub fn children(&self, id: &str) -> Vec<Context> {
        self.pool.lock().unwrap().children(id).into_iter().cloned().collect()
    }

    // -- Write methods (disk first, then pool) --

    /// Write a message to the file associated with a session.
    pub fn write_message(
        &self,
        session_id: &SessionId,
        role: Role,
        content: Vec<ContentBlock>,
        meta: Option<serde_json::Value>,
    ) -> eyre::Result<Message> {
        let file_stem = self.resolve_file(session_id)?;
        let id = MessageId::new(gen_obj_id());
        let msg = Message { id, role, content, meta };

        let line = serde_json::to_string(&MessageLine {
            msg: msg.id.clone(),
            role: msg.role,
            content: msg.content.clone(),
            meta: msg.meta.clone(),
        })?;

        self.append_line(&file_stem, &line)?;

        tracing::debug!("Wrote {:?} message [{}] to [{}]", msg.role, msg.id, file_stem);
        self.pool.lock().unwrap().put_message(msg.clone());
        Ok(msg)
    }

    /// Write a context to the file associated with a session.
    ///
    /// Does NOT update the session's head pointer. Call `update_head`
    /// separately, or use `checkpoint` for the common
    /// write-context-and-advance-head pattern.
    pub fn write_context(
        &self,
        session_id: &SessionId,
        messages: Vec<MessageId>,
        parents: Vec<ContextId>,
        meta: Option<serde_json::Value>,
    ) -> eyre::Result<Context> {
        let file_stem = self.resolve_file(session_id)?;
        let id = ContextId::new(gen_obj_id());
        let ctx = Context { id, messages, parents, meta };

        let line = serde_json::to_string(&ContextLine {
            context: ctx.id.clone(),
            messages: ctx.messages.clone(),
            parents: ctx.parents.clone(),
            meta: ctx.meta.clone(),
        })?;

        self.append_line(&file_stem, &line)?;

        tracing::debug!("Wrote context [{}] to [{}] ({} messages, {} parents)",
            ctx.id, file_stem, ctx.messages.len(), ctx.parents.len());
        self.pool.lock().unwrap().put_context(ctx.clone());
        Ok(ctx)
    }

    /// Create a new context and update the session's head.
    pub fn checkpoint(
        &self,
        session_id: &SessionId,
        message_ids: &[MessageId],
        meta: Option<serde_json::Value>,
    ) -> eyre::Result<Context> {
        let parents = self.pool.lock().unwrap()
            .get_session(session_id.as_str())
            .map(|s| s.head.clone())
            .into_iter()
            .collect();
        let ctx = self.write_context(session_id, message_ids.to_vec(), parents, meta)?;
        self.update_head(session_id, &ctx.id)?;
        Ok(ctx)
    }

    /// Update a session's head and write a full-snapshot session line.
    pub fn update_head(
        &self,
        session_id: &SessionId,
        context_id: &ContextId,
    ) -> eyre::Result<()> {
        let snapshot = {
            let mut pool = self.pool.lock().unwrap();
            let session = pool.sessions.get_mut(session_id.as_str())
                .ok_or_else(|| eyre::eyre!("session '{}' not found in pool", session_id))?;
            session.head = context_id.clone();
            session.clone()
        };
        self.write_session_line(&snapshot)
    }

    /// Update a session's display name and persist it.
    pub fn write_title(
        &self,
        session_id: &SessionId,
        title: &str,
    ) -> eyre::Result<()> {
        let snapshot = {
            let mut pool = self.pool.lock().unwrap();
            let session = pool.sessions.get_mut(session_id.as_str())
                .ok_or_else(|| eyre::eyre!("session '{}' not found in pool", session_id))?;
            session.name = title.to_string();
            session.clone()
        };
        self.write_session_line(&snapshot)
    }

    /// Create a new session.
    ///
    /// If `parent` is `Some`, the new session shares the parent's JSONL file.
    /// Otherwise, creates a new file named after the session.
    ///
    /// Writes a root context and a session pointer line to the file.
    pub fn create_session(
        &self,
        name: &str,
        cwd: &str,
        parent: Option<&SessionId>,
        host: Option<&str>,
    ) -> eyre::Result<SessionId> {
        let now = Utc::now();
        let ts = now.to_rfc3339();

        // Determine file stem: share parent's file if parented, else new file.
        let (file_stem, session_id) = if let Some(parent_id) = parent {
            let parent_file = self.pool.lock().unwrap()
                .get_session(parent_id.as_str())
                .map(|s| s.file.clone())
                .ok_or_else(|| eyre::eyre!(
                    "parent session '{}' not found in pool", parent_id
                ))?;
            let file_ts = now.format("%Y-%m-%d_%H%M%S");
            let slug = slugify(name);
            let rand = &uuid::Uuid::new_v4().simple().to_string()[..4];
            let sid = SessionId::new(format!("{}_{}_{}", file_ts, slug, rand));
            (parent_file, sid)
        } else {
            fs::create_dir_all(&self.sessions_dir)?;
            let file_ts = now.format("%Y-%m-%d_%H%M%S");
            let slug = slugify(name);
            let stem = format!("{}_{}", file_ts, slug);
            (stem.clone(), SessionId::new(stem))
        };

        // Build the root context + session pointer lines.
        let root_id = ContextId::new(gen_obj_id());
        let root_line = serde_json::to_string(&ContextLine {
            context: root_id.clone(),
            messages: Vec::new(),
            parents: Vec::new(),
            meta: None,
        })?;
        let session_line = serde_json::to_string(&SessionLine {
            session: session_id.clone(),
            head: root_id.clone(),
            name: name.to_string(),
            ts: ts.clone(),
            cwd: Some(cwd.to_string()),
            host: host.map(str::to_string),
            parent: parent.cloned(),
        })?;

        // Write both lines atomically (under the same write lock acquisition).
        self.append_lines(&file_stem, &[&root_line, &session_line])?;

        // Register in pool.
        let mut pool = self.pool.lock().unwrap();
        pool.put_context(Context {
            id: root_id.clone(),
            messages: Vec::new(),
            parents: Vec::new(),
            meta: None,
        });
        pool.put_session(Session {
            name: name.to_string(),
            id: session_id.clone(),
            head: root_id,
            cwd: Some(cwd.to_string()),
            host: host.map(str::to_string),
            parent: parent.cloned(),
            ts,
            file: file_stem,
        });

        tracing::info!("Created session [{}] -> [{}]", name, session_id);
        Ok(session_id)
    }

    // -- Internal helpers --

    /// Resolve which file stem a session writes to.
    fn resolve_file(&self, session_id: &SessionId) -> eyre::Result<String> {
        self.pool.lock().unwrap()
            .get_session(session_id.as_str())
            .map(|s| s.file.clone())
            .ok_or_else(|| eyre::eyre!(
                "session '{}' not found in pool (write before create?)", session_id
            ))
    }

    fn file_path(&self, file_stem: &str) -> PathBuf {
        self.sessions_dir.join(format!("{}.jsonl", file_stem))
    }

    /// Append a single JSONL line under the write lock.
    fn append_line(&self, file_stem: &str, line: &str) -> eyre::Result<()> {
        self.append_lines(file_stem, &[line])
    }

    /// Append multiple JSONL lines under a single write lock acquisition.
    /// Used by create_session to write root context + session pointer atomically.
    fn append_lines(&self, file_stem: &str, lines: &[&str]) -> eyre::Result<()> {
        let path = self.file_path(file_stem);
        let _guard = self.write_lock.lock().unwrap();
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        for line in lines {
            writeln!(file, "{}", line)?;
        }
        file.flush()?;
        Ok(())
    }

    /// Append a full-snapshot session line to the session's file.
    fn write_session_line(&self, session: &Session) -> eyre::Result<()> {
        let line = serde_json::to_string(&SessionLine {
            session: session.id.clone(),
            head: session.head.clone(),
            name: session.name.clone(),
            ts: session.ts.clone(),
            cwd: session.cwd.clone(),
            host: session.host.clone(),
            parent: session.parent.clone(),
        })?;
        self.append_line(&session.file, &line)?;
        tracing::debug!("Wrote session line [{}] to [{}]", session.id, session.file);
        Ok(())
    }

    // -- Loading --

    /// Load all .jsonl files into the pool. Called once by `open()`.
    fn load_all(&self) -> eyre::Result<()> {
        if !self.sessions_dir.exists() {
            return Ok(());
        }

        let mut entries: Vec<_> = fs::read_dir(&self.sessions_dir)?
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "jsonl"))
            .collect();
        entries.sort_by(|a, b| a.file_name().cmp(&b.file_name()));

        let file_count = entries.len();
        let mut loaded = self.loaded_files.lock().unwrap();
        for entry in entries {
            if let Err(e) = self.load_file(&entry.path()) {
                tracing::warn!("Failed to load store file {}: {}", entry.path().display(), e);
            }
            if let Some(stem) = entry.path().file_stem().and_then(|s| s.to_str()) {
                loaded.insert(stem.to_string());
            }
        }

        let pool = self.pool.lock().unwrap();
        tracing::info!(
            "Loaded store ({} files, {} messages, {} contexts, {} sessions)",
            file_count, pool.message_count(), pool.context_count(),
            pool.session_count(),
        );

        Ok(())
    }

    fn load_file(&self, path: &Path) -> eyre::Result<()> {
        let file_stem = path.file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();

        let file = File::open(path)?;
        let reader = BufReader::new(file);
        let mut pool = self.pool.lock().unwrap();

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
                    pool.put_session(Session {
                        name: sl.name,
                        id: sl.session,
                        head: sl.head,
                        cwd: sl.cwd,
                        host: sl.host,
                        parent: sl.parent,
                        ts: sl.ts,
                        file: file_stem.clone(),
                    });
                }
            } else if obj.get("msg").is_some() {
                if let Ok(ml) = serde_json::from_value::<MessageLine>(obj) {
                    pool.put_message(Message {
                        id: ml.msg,
                        role: ml.role,
                        content: ml.content,
                        meta: ml.meta,
                    });
                }
            } else if obj.get("context").is_some() {
                if let Ok(cl) = serde_json::from_value::<ContextLine>(obj) {
                    pool.put_context(Context {
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

    /// Check for new JSONL files that appeared since the last load (from
    /// external writers like ri-cli). If found, ingest them into the pool.
    /// Called automatically by `sessions()`.
    fn refresh_if_needed(&self) {
        if !self.sessions_dir.exists() { return; }

        let entries: Vec<_> = match fs::read_dir(&self.sessions_dir) {
            Ok(rd) => rd.filter_map(|e| e.ok())
                .filter(|e| e.path().extension().is_some_and(|ext| ext == "jsonl"))
                .collect(),
            Err(_) => return,
        };

        let mut loaded = self.loaded_files.lock().unwrap();
        let mut new_files = Vec::new();
        for entry in &entries {
            if let Some(stem) = entry.path().file_stem().and_then(|s| s.to_str()) {
                if !loaded.contains(stem) {
                    new_files.push(entry.path());
                    loaded.insert(stem.to_string());
                }
            }
        }
        drop(loaded);

        for path in new_files {
            tracing::info!("Ingesting new store file: {}", path.display());
            if let Err(e) = self.load_file(&path) {
                tracing::warn!("Failed to load new store file {}: {}", path.display(), e);
            }
        }
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
