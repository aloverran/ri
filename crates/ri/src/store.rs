//! Session storage: the pool, the history DAG, and session files.
//!
//! This module owns the in-memory object store (Pool) and its persistence
//! layer (Store). On disk, each session is an append-only JSONL file with
//! four line types:
//!
//! - Session header: `{"session": "name", "ts": "...", ...}`
//! - Message: `{"msg": "m1", "role": "user", "content": [...]}`
//! - Step: `{"step": "s1", "context": ["m1", "m2"], "parents": [], "meta": {...}}`
//! - Head update: `{"head": "s2"}`
//!
//! The session's current state is the last `{"head": ...}` line.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use chrono::Utc;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::message::{ContentBlock, Message, MessageId, Role, SessionId, StepId};

// -- Context --

/// An ordered list of message references. Represents what the LLM sees.
///
/// Just a Vec of message IDs pointing into the pool. Treat it like a value.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Context {
    pub messages: Vec<MessageId>,
}

impl Context {
    pub fn new() -> Self {
        Context { messages: Vec::new() }
    }

    pub fn from_ids(ids: Vec<MessageId>) -> Self {
        Context { messages: ids }
    }

    pub fn len(&self) -> usize {
        self.messages.len()
    }

    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }
}

// -- Step --

/// A point in the history DAG. Records a context snapshot and parent steps.
///
/// Like a git commit: captures *what* the context looks like at this point
/// and *how* it got here (parents). The meta field carries model info, usage,
/// timestamps, or any application-specific data.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Step {
    pub id: StepId,
    pub context: Context,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parents: Vec<StepId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<serde_json::Value>,
}

// -- Pool --

/// The shared object store. Messages and steps live here, referenced by ID.
///
/// The pool doesn't know about sessions or files. It's a bag of objects
/// with lookup by ID. The Store layer populates it from disk and writes
/// new objects to session files.
pub struct Pool {
    messages: HashMap<MessageId, Message>,
    steps: HashMap<StepId, Step>,
}

impl Pool {
    pub fn new() -> Self {
        Pool {
            messages: HashMap::new(),
            steps: HashMap::new(),
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

    // -- Messages (write, crate-internal) --

    pub(crate) fn put_message(&mut self, msg: Message) {
        assert!(!msg.id.as_str().is_empty(), "message ID must not be empty (role={:?})", msg.role);
        self.messages.insert(msg.id.clone(), msg);
    }

    pub(crate) fn message_count(&self) -> usize {
        self.messages.len()
    }

    // -- Steps (read) --

    pub fn get_step(&self, id: &str) -> Option<&Step> {
        self.steps.get(id)
    }

    // -- Steps (write, crate-internal) --

    pub(crate) fn put_step(&mut self, step: Step) {
        assert!(!step.id.as_str().is_empty(), "step ID must not be empty");
        self.steps.insert(step.id.clone(), step);
    }

    pub(crate) fn step_count(&self) -> usize {
        self.steps.len()
    }
}

impl Default for Pool {
    fn default() -> Self {
        Self::new()
    }
}

// -- Session (the pointer) --

/// A named pointer to a step. Like a git branch.
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
    /// Current step this session points to.
    pub head: StepId,
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

/// A step line in the JSONL file.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct StepLine {
    step: StepId,
    context: Vec<MessageId>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    parents: Vec<StepId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    meta: Option<serde_json::Value>,
}

/// A head-update line. The last one in the file wins.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct HeadLine {
    head: StepId,
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
/// files and writes new messages, steps, and head updates.
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
            "Loaded session history ({} files, {} messages, {} steps)",
            file_count, self.pool.message_count(), self.pool.step_count(),
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
        let mut head: Option<StepId> = None;

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
            } else if obj.get("step").is_some() {
                if let Ok(sl) = serde_json::from_value::<StepLine>(obj) {
                    self.pool.put_step(Step {
                        id: sl.step,
                        context: Context::from_ids(sl.context),
                        parents: sl.parents,
                        meta: sl.meta,
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

    /// Create a new session file with an initial root step (empty context).
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

        // Write header, root step, and head pointer.
        let root_step_id = StepId::new(scan_next_id(&path, "step"));
        let root_step = StepLine {
            step: root_step_id.clone(),
            context: Vec::new(),
            parents: Vec::new(),
            meta: None,
        };
        let head_line = HeadLine { head: root_step_id.clone() };

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        writeln!(file, "{}", serde_json::to_string(&header)?)?;
        writeln!(file, "{}", serde_json::to_string(&root_step)?)?;
        writeln!(file, "{}", serde_json::to_string(&head_line)?)?;

        // Register in pool and session map.
        self.pool.put_step(Step {
            id: root_step_id.clone(),
            context: Context::new(),
            parents: Vec::new(),
            meta: None,
        });

        let session = Session {
            name: name.to_string(),
            file_id: file_id.clone(),
            head: root_step_id,
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

    /// Snapshot the current context as a new step and update the session's head.
    ///
    /// This is the primary persistence mechanism: after any meaningful change
    /// to the message list, call checkpoint to record the state. On reload,
    /// the head step's context gives back exactly this list.
    pub fn checkpoint(
        &mut self,
        session_id: &SessionId,
        message_ids: &[MessageId],
        meta: Option<serde_json::Value>,
    ) -> eyre::Result<Step> {
        let context = Context::from_ids(message_ids.to_vec());
        let parents = self.sessions.get(session_id.as_str())
            .map(|s| s.head.clone())
            .into_iter()
            .collect();
        self.write_step(session_id, context, parents, meta)
    }

    /// Convenience: get the current context for a session (from its head step).
    pub fn head_context(&self, session_id: &str) -> Option<&Context> {
        let session = self.sessions.get(session_id)?;
        let step = self.pool.get_step(session.head.as_str())?;
        Some(&step.context)
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

    // -- Internal writing helpers --

    /// Write a step to a session file, add it to the pool, and update the
    /// session's head pointer.
    fn write_step(
        &mut self,
        session_id: &SessionId,
        context: Context,
        parents: Vec<StepId>,
        meta: Option<serde_json::Value>,
    ) -> eyre::Result<Step> {
        let path = self.sessions_dir.join(format!("{}.jsonl", session_id));
        let id = StepId::new(scan_next_id(&path, "step"));

        let step = Step { id, context, parents, meta };

        let step_line = StepLine {
            step: step.id.clone(),
            context: step.context.messages.clone(),
            parents: step.parents.clone(),
            meta: step.meta.clone(),
        };
        let head_line = HeadLine { head: step.id.clone() };

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        writeln!(file, "{}", serde_json::to_string(&step_line)?)?;
        writeln!(file, "{}", serde_json::to_string(&head_line)?)?;
        file.flush()?;

        tracing::debug!("Wrote step [{}] to session [{}] (context: {} messages, {} parents)",
            step.id, session_id, step.context.len(), step.parents.len());

        self.pool.put_step(step.clone());
        if let Some(session) = self.sessions.get_mut(session_id.as_str()) {
            session.head = step.id.clone();
        }

        Ok(step)
    }
}

// -- ID scanning --

/// Scan a session file for IDs matching `{prefix}_{N}` pattern and return
/// the next available ID.
fn scan_next_id(path: &Path, kind: &str) -> String {
    let needle = format!("\"{}\":\"", kind);
    let mut max_counter: u64 = 0;
    let mut prefix: Option<String> = None;

    if let Ok(file) = File::open(path) {
        let reader = BufReader::new(file);
        for line in reader.lines().flatten() {
            let trimmed = line.trim();
            if trimmed.is_empty() { continue; }
            if !trimmed.contains(&needle) { continue; }

            if let Some(id) = extract_field_value(trimmed, &needle) {
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
