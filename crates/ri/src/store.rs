use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use chrono::Utc;
use serde::{Serialize, Deserialize};

use crate::JsonMap;
use crate::message::{gen_session_prefix, ContentBlock, Message, MessagePool, Provenance, Role};

/// Session header -- serialized as the first line of a .jsonl session file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionHeader {
    pub session: String,
    pub ts: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// File-stem ID of the parent session, if this session was spawned by another.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    #[serde(flatten)]
    pub extra: JsonMap,
}

/// Manages the message pool and session files. Loads history from
/// existing .jsonl files and writes new messages to any session by ID.
///
/// The store holds no mutable per-session state. Each write opens the
/// session file, scans it for the next available message ID, appends one
/// JSONL line, and closes. The file is the source of truth.
pub struct SessionStore {
    pub pool: MessagePool,
    sessions_dir: PathBuf,
}

impl SessionStore {
    pub fn new(sessions_dir: PathBuf) -> Self {
        SessionStore {
            pool: MessagePool::new(),
            sessions_dir,
        }
    }

    pub fn default_dir() -> eyre::Result<PathBuf> {
        let home = dirs::home_dir()
            .ok_or_else(|| eyre::eyre!("Could not determine home directory"))?;
        Ok(home.join(".ri").join("sessions"))
    }

    /// Load all .jsonl session files from the sessions directory into the pool.
    pub fn load_all(&mut self) -> eyre::Result<()> {
        if !self.sessions_dir.exists() {
            return Ok(());
        }

        let mut entries: Vec<_> = fs::read_dir(&self.sessions_dir)?
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.path().extension()
                    .is_some_and(|ext| ext == "jsonl")
            })
            .collect();

        entries.sort_by(|a, b| a.file_name().cmp(&b.file_name()));

        let files = entries.len();
        for entry in entries {
            if let Err(e) = self.load_file(&entry.path()) {
                tracing::warn!("Failed to load session file {}: {}", entry.path().display(), e);
            }
        }

        let messages = self.pool.len();
        tracing::info!("Loaded session history ({files} files, {messages} messages)");

        Ok(())
    }

    fn load_file(&mut self, path: &Path) -> eyre::Result<()> {
        let file = File::open(path)?;
        let reader = BufReader::new(file);
        let mut first_line = true;

        for (line_num, line) in reader.lines().enumerate() {
            let line = match line {
                Ok(l) => l,
                Err(e) => {
                    tracing::warn!("{}:{}: read error: {}", path.display(), line_num + 1, e);
                    break;
                }
            };

            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            if first_line {
                first_line = false;
                // Detect header structurally: has "session" key, no "role" key.
                if let Ok(obj) = serde_json::from_str::<serde_json::Value>(trimmed) {
                    if obj.get("session").is_some() && obj.get("role").is_none() {
                        continue;
                    }
                }
            }

            match serde_json::from_str::<Message>(trimmed) {
                Ok(msg) => {
                    self.pool.put(msg);
                }
                Err(e) => {
                    tracing::warn!("{}:{}: malformed message, skipping: {}", path.display(), line_num + 1, e);
                }
            }
        }

        Ok(())
    }

    /// Create a new session file. Returns the session ID (an opaque string,
    /// currently a timestamp-based file stem like `"2026-02-24_201128_my-task"`).
    ///
    /// Pass `initial_ids` to record cross-session message references in the
    /// header (e.g. parent context passed to a sub-agent). These are persisted
    /// so that `readSession` can resolve them later.
    pub fn create_session(
        &mut self,
        name: &str,
        cwd: &str,
        parent: Option<&str>,
        initial_ids: &[String],
    ) -> eyre::Result<String> {
        fs::create_dir_all(&self.sessions_dir)?;

        let now = Utc::now();
        let ts = now.to_rfc3339();
        let file_ts = now.format("%Y-%m-%d_%H%M%S");
        let slug = slugify(name);
        let filename = format!("{}_{}.jsonl", file_ts, slug);
        let path = self.sessions_dir.join(&filename);

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;

        let mut header = SessionHeader {
            session: name.to_string(),
            ts,
            cwd: Some(cwd.to_string()),
            parent: parent.map(str::to_string),
            extra: Default::default(),
        };
        if !initial_ids.is_empty() {
            header.extra.insert(
                "initial_ids".to_string(),
                serde_json::to_value(initial_ids)?,
            );
        }
        writeln!(file, "{}", serde_json::to_string(&header)?)?;

        let session_id = path.file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();

        tracing::info!("Created session [{}] -> [{}]", name, session_id);
        Ok(session_id)
    }

    /// Append a message to a session file and add it to the pool.
    ///
    /// This is the canonical constructor for Message. The caller provides
    /// the content; the store assigns the sequential ID automatically.
    /// Provide provenance for LLM-derived messages, meta for any extra data.
    pub fn write_message(
        &mut self,
        session_id: &str,
        role: Role,
        content: Vec<ContentBlock>,
        provenance: Option<Provenance>,
        meta: Option<serde_json::Value>,
    ) -> eyre::Result<Message> {
        let path = self.sessions_dir.join(format!("{}.jsonl", session_id));
        let id = scan_next_id(&path);

        tracing::debug!("Writing {:?} message [{}] to session [{}]", role, id, session_id);

        let msg = Message { id, role, content, provenance, meta };

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        writeln!(file, "{}", serde_json::to_string(&msg)?)?;
        file.flush()?;

        self.pool.put(msg.clone());
        Ok(msg)
    }
}

/// Scan a session file for message IDs matching the `prefix_N` pattern.
/// Returns the next available ID (`prefix_{max+1}`). If no messages exist
/// yet, generates a fresh prefix from the filename.
fn scan_next_id(path: &Path) -> String {
    let mut max_counter: u64 = 0;
    let mut prefix: Option<String> = None;

    if let Ok(file) = File::open(path) {
        let reader = BufReader::new(file);
        for line in reader.lines().flatten() {
            let trimmed = line.trim();
            if trimmed.is_empty() { continue; }
            // Quick reject: skip lines without "id" (headers, malformed).
            if !trimmed.contains("\"id\"") { continue; }
            if let Some(id) = extract_id(trimmed) {
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
        // Derive a prefix from the filename (the session name portion).
        let stem = path.file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("s");
        gen_session_prefix(stem)
    });

    format!("{}_{}", prefix, max_counter + 1)
}

/// Fast ID extraction without full JSON parse -- just find the "id" field value.
fn extract_id(line: &str) -> Option<&str> {
    // Look for "id":"<value>" pattern. Avoids serde_json for speed on large files.
    let needle = "\"id\":\"";
    let start = line.find(needle)? + needle.len();
    let end = start + line[start..].find('"')?;
    Some(&line[start..end])
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
