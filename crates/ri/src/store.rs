//! Pool, Store, and mount management: the storage layer over the three
//! core primitives.
//!
//! `Pool` is the in-memory DAG -- a unified view of every message,
//! context, and ref currently loaded, backed by one or more filesystem
//! directories. Reads are always global across the pool: messages,
//! contexts, and refs are all global atoms with no notion of "which
//! mount or file they belong to".
//!
//! `Store` is a write handle bound to one mount *and one segment*. A
//! `Segment` is a relative path within the mount -- a write goes to
//! `<mount>/<segment>.jsonl`. The default segment is `store`, so a plain
//! mount writes to `<mount>/store.jsonl` exactly as before. Rebinding to
//! another segment (`store.segment("ref_2604_ab")`) targets a different
//! file in the same mount; reads still resolve globally through the pool.
//!
//! Core attaches no meaning to the segment string. Applications choose
//! the layout: chat keeps a flat file per session family (segment = the
//! root ref id), while banks nest a folder per bank (segment =
//! `<bank>/<unit>`). Because the segment can contain `/`, one mount holds
//! both flat files and folder trees, and deleting a tree is a plain `rm`.
//!
//! On disk, the loader walks the whole mount directory recursively and
//! ingests every `*.jsonl` it finds, at any depth. Each mount serializes
//! appends per file: two writers to the same segment can't interleave
//! lines, but writers to different segments proceed in parallel.
//!
//! Refs are mutable: every write appends a full snapshot line carrying a
//! wall-clock `ts`. The loader keeps the highest-ts line per RefId,
//! independent of file load order or mount layout. Cross-process,
//! cross-mount, and post-rename writes all converge deterministically.
//!
//! ```text
//! pool = Pool::new();
//! sessions = pool.mount("~/.ri/sessions")?;  // Store on the default segment
//! family   = sessions.segment("ref_2604_ab")?; // a per-family file
//!
//! let msg = Message::new(Role::User, content, None);
//! family.write_message(&msg)?;               // -> sessions/ref_2604_ab.jsonl
//!
//! pool.get_message(&msg.id);                 // resolves globally
//! pool.refs();                               // every ref, every mount
//! ```

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::model::{
    ContentBlock, Context, ContextId, Message, MessageId, Ref, RefId, Role,
};

// -- Pool ---------------------------------------------------------------

/// In-memory DAG of every atom currently loaded. Cheap to clone (internal
/// `Arc`); clones are handles, not copies.
///
/// All three atoms (messages, contexts, refs) live globally in the pool
/// and resolve regardless of which mount or file physically holds them.
/// The pool also tracks the wall-clock timestamp of each ref's most
/// recently loaded line so cross-mount and cross-process writes converge
/// to the freshest snapshot.
#[derive(Clone)]
pub struct Pool {
    inner: Arc<Mutex<PoolInner>>,
}

struct PoolInner {
    messages: HashMap<MessageId, Message>,
    contexts: HashMap<ContextId, Context>,
    refs: HashMap<RefId, Ref>,
    /// Wall-clock ts of the line that produced each ref's current state.
    /// Used to discard older snapshots when the same RefId appears in
    /// multiple files or mounts.
    ref_ts: HashMap<RefId, DateTime<Utc>>,
    /// Mount directory plus its per-file write locks.
    mounts: HashMap<MountId, Mount>,
    next_mount: u64,
}

/// Opaque per-mount handle. Stores carry one to identify their write
/// target; nothing in the user-facing API needs to construct or compare
/// these.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MountId(u64);

struct Mount {
    dir: PathBuf,
    /// One append lock per segment, created lazily on first write. Holds
    /// only during serialize + write + flush. Two stores targeting the
    /// same segment share the lock (lines never interleave); stores on
    /// different segments hold different locks and write concurrently.
    locks: Mutex<HashMap<Segment, Arc<Mutex<()>>>>,
}

impl Pool {
    pub fn new() -> Self {
        Pool {
            inner: Arc::new(Mutex::new(PoolInner {
                messages: HashMap::new(),
                contexts: HashMap::new(),
                refs: HashMap::new(),
                ref_ts: HashMap::new(),
                mounts: HashMap::new(),
                next_mount: 0,
            })),
        }
    }

    /// Attach a directory to the pool. Recursively loads every `*.jsonl`
    /// file under the path into the shared DAG and returns a write-scoped
    /// `Store` bound to the new mount's default segment (`store.jsonl`).
    ///
    /// File load order doesn't matter for correctness: every ref line
    /// carries a `ts` and the loader keeps the newest.
    ///
    /// Mounting the same canonicalized directory twice is rejected: it
    /// would create two independent lock tables against one set of files,
    /// silently breaking append serialization. The error surfaces the
    /// misconfiguration before any writes happen.
    pub fn mount(&self, path: impl AsRef<Path>) -> eyre::Result<Store> {
        let dir: PathBuf = path.as_ref().to_path_buf();
        fs::create_dir_all(&dir)?;
        let canonical = fs::canonicalize(&dir)?;

        let mount_id = {
            let mut inner = self.inner.lock().unwrap();
            for (existing_id, existing) in &inner.mounts {
                if let Ok(existing_canonical) = fs::canonicalize(&existing.dir) {
                    if existing_canonical == canonical {
                        return Err(eyre::eyre!(
                            "directory {} is already mounted (mount {:?})",
                            canonical.display(), existing_id
                        ));
                    }
                }
            }
            let id = MountId(inner.next_mount);
            inner.next_mount += 1;
            inner.mounts.insert(id, Mount {
                dir: dir.clone(),
                locks: Mutex::new(HashMap::new()),
            });
            id
        };

        let store = Store { pool: self.clone(), mount: mount_id, segment: Segment::default() };
        store.load_mount(&dir)?;
        Ok(store)
    }

    pub fn get_message(&self, id: &MessageId) -> Option<Message> {
        self.inner.lock().unwrap().messages.get(id).cloned()
    }

    pub fn get_context(&self, id: &ContextId) -> Option<Context> {
        self.inner.lock().unwrap().contexts.get(id).cloned()
    }

    pub fn get_ref(&self, id: &RefId) -> Option<Ref> {
        self.inner.lock().unwrap().refs.get(id).cloned()
    }

    /// Convenience: fetch the context a ref currently points at. `None`
    /// if either the ref or its head context is missing from the pool.
    pub fn head_context(&self, ref_id: &RefId) -> Option<Context> {
        let inner = self.inner.lock().unwrap();
        let r = inner.refs.get(ref_id)?;
        inner.contexts.get(&r.head).cloned()
    }

    /// All refs currently loaded, across every mount. Apps filter this
    /// by facet (`r.facet::<ChatFacet>()`, `r.facet::<BankFacet>()`,
    /// ...) to get the subset they care about.
    pub fn refs(&self) -> Vec<Ref> {
        self.inner.lock().unwrap().refs.values().cloned().collect()
    }

    /// Drop a ref from the in-memory pool. Deletion flows call this after
    /// removing the backing file so the ref stops showing up in pool
    /// queries (the session list, lineage walks) without a reload.
    ///
    /// Only the ref is evicted. Any messages and contexts it reached stay
    /// in the pool until the next mount load -- they are immutable and now
    /// unreferenced, so leaving them is harmless, and the pool keeps no
    /// atom->file index to evict them precisely without one.
    pub fn remove_ref(&self, id: &RefId) -> Option<Ref> {
        let mut inner = self.inner.lock().unwrap();
        inner.ref_ts.remove(id);
        inner.refs.remove(id)
    }

    /// Resolve an ordered list of message IDs to their messages. Skips
    /// missing IDs but warns -- unresolved ids are usually a bug worth
    /// surfacing.
    pub fn resolve(&self, ids: &[MessageId]) -> Vec<Message> {
        let inner = self.inner.lock().unwrap();
        ids.iter().filter_map(|id| {
            let msg = inner.messages.get(id).cloned();
            if msg.is_none() {
                tracing::warn!("Message [{}] not found during context resolution", id);
            }
            msg
        }).collect()
    }

    /// Forward traversal: contexts whose parents include the given id.
    /// O(n) scan over every context in the pool.
    pub fn children(&self, id: &ContextId) -> Vec<Context> {
        self.inner.lock().unwrap().contexts.values()
            .filter(|ctx| ctx.parents.contains(id))
            .cloned()
            .collect()
    }

    pub fn message_count(&self) -> usize { self.inner.lock().unwrap().messages.len() }
    pub fn context_count(&self) -> usize { self.inner.lock().unwrap().contexts.len() }
    pub fn ref_count(&self) -> usize { self.inner.lock().unwrap().refs.len() }
}

impl Default for Pool {
    fn default() -> Self { Self::new() }
}

// -- Segment ------------------------------------------------------------

/// Identifies which file within a mount a write targets: 1:1 with
/// `<mount>/<segment>.jsonl`. The path may contain `/` to nest into
/// subdirectories, so one mount can hold both flat per-family files and
/// per-subsystem folder trees.
///
/// Core attaches no meaning to the path -- the application picks it (the
/// root ref id of a session family, `<bank>/<unit>` for a memory bank,
/// ...). `Segment::new` rejects any path that could escape the mount, so
/// an illegal stem is a programming error surfaced at construction rather
/// than a silent write outside the directory.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Segment(Arc<str>);

impl Segment {
    /// Build a segment from a `/`-separated relative path. Each component
    /// must be non-empty and not `.` or `..`, and may not contain a
    /// backslash or NUL. This forbids absolute paths, parent-directory
    /// escapes, and `//`, so the result always stays inside the mount.
    pub fn new(path: impl AsRef<str>) -> eyre::Result<Self> {
        let p = path.as_ref();
        if p.is_empty() {
            eyre::bail!("segment path is empty");
        }
        for comp in p.split('/') {
            if comp.is_empty() {
                eyre::bail!("segment {p:?} has an empty path component (leading, trailing, or doubled '/')");
            }
            if comp == "." || comp == ".." {
                eyre::bail!("segment {p:?} contains a '.' or '..' component");
            }
            if comp.contains('\\') || comp.contains('\0') || comp.contains(':') {
                eyre::bail!("segment {p:?} contains an illegal character (backslash, colon, or NUL)");
            }
        }
        Ok(Segment(Arc::from(p)))
    }

    /// Resolve to the absolute `<dir>/<path>.jsonl` file. The `.jsonl`
    /// extension lands on the final component; intermediate components
    /// become subdirectories.
    fn file_path(&self, dir: &Path) -> PathBuf {
        let mut path = dir.to_path_buf();
        let mut comps = self.0.split('/').peekable();
        while let Some(comp) = comps.next() {
            if comps.peek().is_some() {
                path.push(comp);
            } else {
                path.push(format!("{comp}.jsonl"));
            }
        }
        path
    }
}

/// The mount-wide catch-all (`store.jsonl`). A freshly mounted store
/// writes here until rebound with `Store::segment`, which preserves the
/// historical single-file behavior.
impl Default for Segment {
    fn default() -> Self { Segment(Arc::from("store")) }
}

// -- Store --------------------------------------------------------------

/// A mount-and-segment-scoped write handle on a pool. Writes append to
/// this segment's `.jsonl`; reads delegate to the pool, so they remain
/// global. Cheap to clone (holds a pool clone, a mount id, and a shared
/// segment string).
///
/// Stores never expose mount ids or file paths to their callers. A
/// subsystem receives a store and treats it as an opaque DB handle.
#[derive(Clone)]
pub struct Store {
    pool: Pool,
    mount: MountId,
    segment: Segment,
}

impl Store {
    /// Write a message. The atom is appended to this store's segment file
    /// and registered in the pool.
    pub fn write_message(&self, msg: &Message) -> eyre::Result<()> {
        let line = serde_json::to_string(&MessageLine {
            msg: msg.id.clone(),
            role: msg.role,
            content: msg.content.clone(),
            meta: msg.meta.clone(),
        })?;
        self.append_line(&line)?;
        self.pool.inner.lock().unwrap().messages.insert(msg.id.clone(), msg.clone());
        tracing::debug!("Wrote {:?} message [{}] to mount {:?} segment [{}]", msg.role, msg.id, self.mount, self.segment.0);
        Ok(())
    }

    /// Write a context. Immutable by convention: writing the same id
    /// twice is a usage error but not structurally prevented.
    pub fn write_context(&self, ctx: &Context) -> eyre::Result<()> {
        let line = serde_json::to_string(&ContextLine {
            context: ctx.id.clone(),
            messages: ctx.messages.clone(),
            parents: ctx.parents.clone(),
            meta: ctx.meta.clone(),
        })?;
        self.append_line(&line)?;
        self.pool.inner.lock().unwrap().contexts.insert(ctx.id.clone(), ctx.clone());
        tracing::debug!("Wrote context [{}] to mount {:?} segment [{}] ({} messages, {} parents)",
            ctx.id, self.mount, self.segment.0, ctx.messages.len(), ctx.parents.len());
        Ok(())
    }

    /// Write a ref. Refs are mutable: writing the same id again appends
    /// a new full-snapshot line stamped with the current wall clock and
    /// supersedes any earlier snapshot on the next load. In-memory state
    /// updates immediately.
    pub fn write_ref(&self, r: &Ref) -> eyre::Result<()> {
        let ts = Utc::now();
        let line = serde_json::to_string(&RefLine {
            r#ref: r.id.clone(),
            head: r.head.clone(),
            ts,
            meta: r.meta.clone(),
        })?;
        self.append_line(&line)?;
        let mut inner = self.pool.inner.lock().unwrap();
        inner.refs.insert(r.id.clone(), r.clone());
        inner.ref_ts.insert(r.id.clone(), ts);
        tracing::debug!("Wrote ref [{}] -> [{}] to mount {:?} segment [{}]", r.id, r.head, self.mount, self.segment.0);
        Ok(())
    }

    /// A handle to a different file within the same mount and pool. The
    /// returned store writes to `<mount>/<segment>.jsonl`; reads are
    /// unchanged (always global through the pool). Cheap clone.
    ///
    /// Errors only if the segment path is malformed (could escape the
    /// mount); see [`Segment::new`].
    pub fn segment(&self, path: impl AsRef<str>) -> eyre::Result<Store> {
        Ok(Store {
            pool: self.pool.clone(),
            mount: self.mount,
            segment: Segment::new(path)?,
        })
    }

    /// Delete this store's segment file from disk. Returns whether a file
    /// was actually removed (a missing file is not an error -- deletion is
    /// idempotent). Holds the segment's append lock during removal so a
    /// concurrent write can't interleave with the delete.
    ///
    /// In-memory eviction is a separate concern: the pool keeps no
    /// atom->file index, so the caller drops the relevant refs via
    /// [`Pool::remove_ref`]. Lingering messages/contexts are immutable and
    /// unreferenced once their refs are gone, so they're harmless until the
    /// next mount load clears them.
    pub fn delete_segment_file(&self) -> eyre::Result<bool> {
        let (path, lock) = self.mount_target()?;
        let _guard = lock.lock().unwrap();
        match fs::remove_file(&path) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    pub fn pool(&self) -> &Pool { &self.pool }

    pub fn get_message(&self, id: &MessageId) -> Option<Message> { self.pool.get_message(id) }
    pub fn get_context(&self, id: &ContextId) -> Option<Context> { self.pool.get_context(id) }
    pub fn get_ref(&self, id: &RefId) -> Option<Ref> { self.pool.get_ref(id) }

    /// Shorthand for `pool.head_context(ref_id)`. Widely used by chat
    /// apps so worth the four lines.
    pub fn head_context(&self, ref_id: &RefId) -> Option<Context> {
        self.pool.head_context(ref_id)
    }

    /// Shorthand for `pool.children(id)`.
    pub fn children(&self, id: &ContextId) -> Vec<Context> {
        self.pool.children(id)
    }

    // -- internals --

    fn append_line(&self, line: &str) -> eyre::Result<()> {
        let (path, lock) = self.mount_target()?;
        let _guard = lock.lock().unwrap();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
        writeln!(file, "{}", line)?;
        file.flush()?;
        Ok(())
    }

    /// Resolve this store's `(file path, append lock)`. The lock is shared
    /// across every store targeting the same `(mount, segment)` and is
    /// created on first use. Neither the pool lock nor the lock-table lock
    /// is held across file I/O.
    fn mount_target(&self) -> eyre::Result<(PathBuf, Arc<Mutex<()>>)> {
        let inner = self.pool.inner.lock().unwrap();
        let mount = inner.mounts.get(&self.mount)
            .ok_or_else(|| eyre::eyre!("mount {:?} missing from pool", self.mount))?;
        let path = self.segment.file_path(&mount.dir);
        let lock = mount.locks.lock().unwrap()
            .entry(self.segment.clone())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone();
        Ok((path, lock))
    }

    fn load_mount(&self, dir: &Path) -> eyre::Result<()> {
        if !dir.exists() { return Ok(()); }

        // Gather every *.jsonl under the mount, at any depth, then load in
        // a stable order. Order is irrelevant to correctness (refs carry a
        // `ts` and supersede by recency); the sort just keeps log lines
        // and any same-ts tie-breaks deterministic.
        let mut files: Vec<PathBuf> = Vec::new();
        collect_jsonl(dir, &mut files, 0);
        files.sort();

        for path in &files {
            if let Err(e) = self.load_file(path) {
                tracing::warn!("Failed to load store file {}: {}", path.display(), e);
            }
        }

        let inner = self.pool.inner.lock().unwrap();
        tracing::info!(
            "Mounted {} ({} files, {} messages, {} contexts, {} refs)",
            dir.display(), files.len(),
            inner.messages.len(), inner.contexts.len(), inner.refs.len(),
        );
        Ok(())
    }

    fn load_file(&self, path: &Path) -> eyre::Result<()> {
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

            if let Err(e) = self.ingest_line(obj) {
                tracing::warn!("{}:{}: {}", path.display(), line_num + 1, e);
            }
        }
        Ok(())
    }

    fn ingest_line(&self, obj: serde_json::Value) -> eyre::Result<()> {
        let mut inner = self.pool.inner.lock().unwrap();
        // Four line types: new ref, legacy session, message, context.
        // Legacy sessions have both "session" + "head" keys; new refs
        // have both "ref" + "head" keys.
        if obj.get("ref").is_some() && obj.get("head").is_some() {
            let rl: RefLine = serde_json::from_value(obj)?;
            install_ref(
                &mut inner,
                Ref { id: rl.r#ref.clone(), head: rl.head, meta: rl.meta },
                rl.ts,
            );
        } else if obj.get("session").is_some() && obj.get("head").is_some() {
            let sl: LegacySessionLine = serde_json::from_value(obj)?;
            let id = RefId::new(sl.session.clone());
            let ts = parse_legacy_ts(sl.ts.as_deref());
            let meta = Some(synth_chat_meta(&sl));
            install_ref(&mut inner, Ref { id, head: sl.head, meta }, ts);
        } else if obj.get("msg").is_some() {
            let ml: MessageLine = serde_json::from_value(obj)?;
            inner.messages.insert(ml.msg.clone(), Message {
                id: ml.msg, role: ml.role, content: ml.content, meta: ml.meta,
            });
        } else if obj.get("context").is_some() {
            let cl: ContextLine = serde_json::from_value(obj)?;
            inner.contexts.insert(cl.context.clone(), Context {
                id: cl.context, messages: cl.messages, parents: cl.parents, meta: cl.meta,
            });
        } else {
            return Err(eyre::eyre!("unrecognized line format"));
        }
        Ok(())
    }
}

/// Recursively gather every `*.jsonl` file under `dir` into `out`.
///
/// Directory symlinks are not followed: `DirEntry::file_type` reports the
/// entry itself (not its target), so a symlinked directory reads as a
/// symlink rather than a dir and is skipped -- the walk can't loop. A
/// generous depth cap is a second guard against pathological trees.
fn collect_jsonl(dir: &Path, out: &mut Vec<PathBuf>, depth: usize) {
    const MAX_DEPTH: usize = 16;
    if depth > MAX_DEPTH {
        tracing::warn!("Stopping mount load at depth {} under {}", MAX_DEPTH, dir.display());
        return;
    }
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!("Failed to read dir {}: {}", dir.display(), e);
            return;
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_dir() {
            collect_jsonl(&path, out, depth + 1);
        } else if ft.is_file() && path.extension().is_some_and(|x| x == "jsonl") {
            out.push(path);
        }
    }
}

/// Install a ref snapshot iff its `ts` is at least as new as anything
/// we've already loaded for the same id. Equal-ts ties go to the
/// later-arriving line so a same-second double-write still converges
/// without a coin flip in the parser.
fn install_ref(inner: &mut PoolInner, r: Ref, ts: DateTime<Utc>) {
    if let Some(existing_ts) = inner.ref_ts.get(&r.id) {
        if ts < *existing_ts {
            return;
        }
    }
    inner.ref_ts.insert(r.id.clone(), ts);
    inner.refs.insert(r.id.clone(), r);
}

/// Parse a legacy session line's `ts` field into a `DateTime<Utc>`.
/// Two formats appear in the wild: the chat facet's
/// `%Y-%m-%d %H:%M:%S UTC` and the older RFC3339 form. Anything
/// unparseable falls back to `MIN_UTC` so a current write always
/// supersedes a legacy line lacking provenance.
fn parse_legacy_ts(s: Option<&str>) -> DateTime<Utc> {
    let Some(s) = s.map(str::trim).filter(|s| !s.is_empty()) else {
        return Utc.timestamp_opt(0, 0).single().unwrap_or_else(Utc::now);
    };
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return dt.with_timezone(&Utc);
    }
    if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S UTC") {
        return Utc.from_utc_datetime(&naive);
    }
    Utc.timestamp_opt(0, 0).single().unwrap_or_else(Utc::now)
}

// -- On-disk line formats -----------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MessageLine {
    msg: MessageId,
    role: Role,
    content: Vec<ContentBlock>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    meta: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ContextLine {
    context: ContextId,
    messages: Vec<MessageId>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    parents: Vec<ContextId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    meta: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RefLine {
    r#ref: RefId,
    head: ContextId,
    /// Wall-clock of when this snapshot was written. Used at load time
    /// to merge multiple snapshots for the same RefId by recency.
    ts: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    meta: Option<serde_json::Value>,
}

/// Legacy Session line shape. Loader translates this into a Ref with
/// its own synthesised `chat` facet.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct LegacySessionLine {
    session: String,
    head: ContextId,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    ts: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    host: Option<String>,
    #[serde(default)]
    parent: Option<String>,
}

/// Build the `chat` facet payload from a legacy session line. Applications
/// that read the `chat` facet get the same shape whether the ref was
/// freshly created or loaded from a legacy line.
fn synth_chat_meta(sl: &LegacySessionLine) -> serde_json::Value {
    json!({
        "chat": {
            "title": sl.name.clone().unwrap_or_default(),
            "created_at": sl.ts.clone().unwrap_or_default(),
            "cwd": sl.cwd.clone().unwrap_or_default(),
            "host": sl.host.clone(),
            "parent": sl.parent.clone(),
        }
    })
}

// -- Convenience helpers ------------------------------------------------

/// Default sessions directory (`~/.ri/sessions`). Kept here so applications
/// don't have to reinvent it.
pub fn default_sessions_dir() -> eyre::Result<PathBuf> {
    let home = dirs::home_dir()
        .ok_or_else(|| eyre::eyre!("Could not determine home directory"))?;
    Ok(home.join(".ri").join("sessions"))
}

/// Default blob directory (`~/.ri/blobs`) -- the global content-addressed
/// store the pool never loads. Kept beside [`default_sessions_dir`].
pub fn default_blobs_dir() -> eyre::Result<PathBuf> {
    let home = dirs::home_dir()
        .ok_or_else(|| eyre::eyre!("Could not determine home directory"))?;
    Ok(home.join(".ri").join("blobs"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(tag: &str) -> PathBuf {
        let unique = uuid::Uuid::new_v4().simple().to_string();
        let dir = std::env::temp_dir().join(format!("ri-store-{}-{}", tag, unique));
        std::fs::create_dir_all(&dir).expect("create tmp dir");
        dir
    }

    /// Two stores, each with a snapshot of the same RefId. The newer
    /// snapshot wins after both have been loaded -- regardless of which
    /// directory got mounted second.
    #[test]
    fn ref_supersession_picks_newest_ts_across_mounts() {
        let dir_a = tmp_dir("a");
        let dir_b = tmp_dir("b");

        let ctx_old = ContextId::new("ctx_old");
        let ctx_new = ContextId::new("ctx_new");
        let ref_id = RefId::new("ref_under_test");

        // Pre-seed each directory with a single ref line. dir_a's line
        // is older; dir_b's line is newer.
        let line_old = serde_json::to_string(&RefLine {
            r#ref: ref_id.clone(),
            head: ctx_old.clone(),
            ts: Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap(),
            meta: None,
        }).unwrap();
        let line_new = serde_json::to_string(&RefLine {
            r#ref: ref_id.clone(),
            head: ctx_new.clone(),
            ts: Utc.with_ymd_and_hms(2026, 4, 1, 0, 0, 0).unwrap(),
            meta: None,
        }).unwrap();
        std::fs::write(dir_a.join("store.jsonl"), format!("{}\n", line_old)).unwrap();
        std::fs::write(dir_b.join("store.jsonl"), format!("{}\n", line_new)).unwrap();

        // Mount A first, then B. Newer line wins.
        let pool = Pool::new();
        let _a = pool.mount(&dir_a).unwrap();
        let _b = pool.mount(&dir_b).unwrap();
        assert_eq!(pool.get_ref(&ref_id).unwrap().head, ctx_new);

        // Mount B first, then A. Same outcome -- order is irrelevant.
        let pool2 = Pool::new();
        let _b2 = pool2.mount(&dir_b).unwrap();
        let _a2 = pool2.mount(&dir_a).unwrap();
        assert_eq!(pool2.get_ref(&ref_id).unwrap().head, ctx_new);
    }

    /// Legacy `session` lines without a `ts` still order against fresh
    /// `ref` lines: a freshly-written ref always supersedes a legacy
    /// snapshot, because the loader assigns legacy lines the epoch
    /// fallback when `ts` is absent.
    #[test]
    fn fresh_ref_supersedes_legacy_session_line() {
        let dir = tmp_dir("legacy");

        let ctx_legacy = ContextId::new("ctx_legacy");
        let ctx_fresh = ContextId::new("ctx_fresh");
        let ref_id = RefId::new("ref_legacy_id");

        let legacy_line = json!({
            "session": ref_id.as_str(),
            "head": ctx_legacy.as_str(),
            "name": "old chat",
        }).to_string();
        std::fs::write(dir.join("legacy.jsonl"), format!("{}\n", legacy_line)).unwrap();

        let pool = Pool::new();
        let store = pool.mount(&dir).unwrap();
        assert_eq!(pool.get_ref(&ref_id).unwrap().head, ctx_legacy);

        let fresh = Ref { id: ref_id.clone(), head: ctx_fresh.clone(), meta: None };
        store.write_ref(&fresh).unwrap();

        // Drop the pool, remount fresh: store.jsonl + legacy.jsonl, the
        // fresh write wins regardless of file iteration order.
        drop(store);
        drop(pool);
        let pool2 = Pool::new();
        let _store2 = pool2.mount(&dir).unwrap();
        assert_eq!(pool2.get_ref(&ref_id).unwrap().head, ctx_fresh);
    }
}
