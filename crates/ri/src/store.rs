//! Pool, Store, and mount management: the storage layer over the three
//! core primitives.
//!
//! `Pool` is the in-memory DAG -- a unified view of every message,
//! context, and ref currently loaded, backed by one or more filesystem
//! directories. Reads are always global across the pool: messages,
//! contexts, and refs are all global atoms with no notion of "which
//! mount they belong to".
//!
//! `Store` is a write handle bound to one mount. Every write a store
//! performs goes to its mount's `store.jsonl`; reads delegate to the
//! pool, so they remain global. A subsystem (chat, bank, ...) receives
//! a `Store` and treats it as an opaque DB handle.
//!
//! On disk, each mount owns one directory. New writes always go to
//! `<mount>/store.jsonl`. Legacy per-session `<slug>.jsonl` files load
//! correctly and are superseded in place by later writes.
//!
//! Refs are mutable: every write appends a full snapshot line carrying
//! a wall-clock `ts`. The loader keeps the highest-ts line per RefId,
//! independent of file load order or mount layout. Cross-process,
//! cross-mount, and post-rename writes all converge deterministically.
//!
//! ```text
//! pool = Pool::new();
//! sessions = pool.mount("~/.ri/sessions")?;  // Store bound to this mount
//! banks    = pool.mount("~/.ri/banks")?;     // another Store, same pool
//!
//! let msg = Message::new(Role::User, content, None);
//! sessions.write_message(&msg)?;             // goes to sessions/store.jsonl
//!
//! let ctx = Context::new(vec![msg.id.clone()], vec![], None);
//! sessions.write_context(&ctx)?;
//!
//! let r = Ref::new(ctx.id.clone(), None);
//! sessions.write_ref(&r)?;
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
/// and resolve regardless of which mount physically holds them. The pool
/// also tracks the wall-clock timestamp of each ref's most recently
/// loaded line so cross-mount and cross-process writes converge to the
/// freshest snapshot.
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
    /// Mount directory plus per-mount write lock.
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
    /// Serializes appends to this mount's `store.jsonl` so lines from
    /// concurrent writers don't interleave. Held only during
    /// serialize + write + flush.
    write_lock: Arc<Mutex<()>>,
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

    /// Attach a directory to the pool. Loads every `*.jsonl` file under
    /// the path into the shared DAG and returns a write-scoped `Store`
    /// bound to the new mount.
    ///
    /// File load order doesn't matter for correctness: every ref line
    /// carries a `ts` and the loader keeps the newest. The legacy-first
    /// pass is purely so the info-line counts read sensibly.
    ///
    /// Mounting the same canonicalized directory twice is rejected:
    /// it would create two write-locks against one file, silently breaking
    /// append serialization. The error surfaces the misconfiguration
    /// before any writes happen.
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
                write_lock: Arc::new(Mutex::new(())),
            });
            id
        };

        let store = Store { pool: self.clone(), mount: mount_id };
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

// -- Store --------------------------------------------------------------

/// A mount-scoped write handle on a pool. Writes append to this mount's
/// `store.jsonl`; reads delegate to the pool, so they remain global.
/// Cheap to clone (holds a pool clone + a mount id).
///
/// Stores never expose mount ids or file paths to their callers. A
/// subsystem receives a store and treats it as an opaque DB handle.
#[derive(Clone)]
pub struct Store {
    pool: Pool,
    mount: MountId,
}

impl Store {
    /// Write a message. The atom is appended to the mount's file and
    /// registered in the pool.
    pub fn write_message(&self, msg: &Message) -> eyre::Result<()> {
        let line = serde_json::to_string(&MessageLine {
            msg: msg.id.clone(),
            role: msg.role,
            content: msg.content.clone(),
            meta: msg.meta.clone(),
        })?;
        self.append_line(&line)?;
        self.pool.inner.lock().unwrap().messages.insert(msg.id.clone(), msg.clone());
        tracing::debug!("Wrote {:?} message [{}] to mount {:?}", msg.role, msg.id, self.mount);
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
        tracing::debug!("Wrote context [{}] to mount {:?} ({} messages, {} parents)",
            ctx.id, self.mount, ctx.messages.len(), ctx.parents.len());
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
        tracing::debug!("Wrote ref [{}] -> [{}] to mount {:?}", r.id, r.head, self.mount);
        Ok(())
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
        let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
        writeln!(file, "{}", line)?;
        file.flush()?;
        Ok(())
    }

    fn mount_target(&self) -> eyre::Result<(PathBuf, Arc<Mutex<()>>)> {
        let inner = self.pool.inner.lock().unwrap();
        let mount = inner.mounts.get(&self.mount)
            .ok_or_else(|| eyre::eyre!("mount {:?} missing from pool", self.mount))?;
        Ok((mount.dir.join(STORE_FILE), mount.write_lock.clone()))
    }

    fn load_mount(&self, dir: &Path) -> eyre::Result<()> {
        if !dir.exists() { return Ok(()); }

        // Two passes: legacy first, then store.jsonl. The split is
        // explicit (not relying on lex order) so renaming legacy files
        // can never silently invert the supersession order.
        let mut legacy: Vec<PathBuf> = Vec::new();
        let mut canonical: Option<PathBuf> = None;
        for entry in fs::read_dir(dir)? {
            let path = match entry {
                Ok(e) => e.path(),
                Err(_) => continue,
            };
            if path.extension().is_some_and(|x| x == "jsonl") {
                if path.file_name().is_some_and(|n| n == STORE_FILE) {
                    canonical = Some(path);
                } else {
                    legacy.push(path);
                }
            }
        }
        legacy.sort();

        let mut loaded_files = 0;
        for path in &legacy {
            if let Err(e) = self.load_file(path) {
                tracing::warn!("Failed to load store file {}: {}", path.display(), e);
            }
            loaded_files += 1;
        }
        if let Some(path) = &canonical {
            if let Err(e) = self.load_file(path) {
                tracing::warn!("Failed to load store file {}: {}", path.display(), e);
            }
            loaded_files += 1;
        }

        let inner = self.pool.inner.lock().unwrap();
        tracing::info!(
            "Mounted {} ({} files, {} messages, {} contexts, {} refs)",
            dir.display(), loaded_files,
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

const STORE_FILE: &str = "store.jsonl";

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
        std::fs::write(dir_a.join(STORE_FILE), format!("{}\n", line_old)).unwrap();
        std::fs::write(dir_b.join(STORE_FILE), format!("{}\n", line_new)).unwrap();

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
