use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use chrono::Utc;

use crate::pool::Pool;
use crate::types::{Message, SessionHeader, SessionInfo};
use crate::id::gen_id;

pub struct SessionFiling {
    pub pool: Pool,
    sessions_dir: PathBuf,
    active: Option<ActiveSession>,
    // Session prefix for generating message IDs in the active session.
    active_prefix: String,
    active_counter: u64,
}

struct ActiveSession {
    file: File,
    path: PathBuf,
    name: String,
}

impl SessionFiling {
    pub fn new(sessions_dir: PathBuf) -> Self {
        SessionFiling {
            pool: Pool::new(),
            sessions_dir,
            active: None,
            active_prefix: String::new(),
            active_counter: 0,
        }
    }

    pub fn default_dir() -> eyre::Result<PathBuf> {
        let home = dirs::home_dir()
            .ok_or_else(|| eyre::eyre!("Could not determine home directory"))?;
        Ok(home.join(".ri").join("sessions"))
    }

    // Load all session files into the pool.
    pub fn load_all(&mut self) -> eyre::Result<()> {
        if !self.sessions_dir.exists() {
            return Ok(());
        }

        let mut entries: Vec<_> = fs::read_dir(&self.sessions_dir)?
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.path().extension()
                    .map_or(false, |ext| ext == "jsonl")
            })
            .collect();

        entries.sort_by(|a, b| a.file_name().cmp(&b.file_name()));

        for entry in entries {
            if let Err(e) = self.load_file(&entry.path()) {
                tracing::warn!("Failed to load session file {}: {}", entry.path().display(), e);
            }
        }

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
                        // It's a session header, not a message.
                        continue;
                    }
                }
            }

            // Parse as message.
            match serde_json::from_str::<Message>(trimmed) {
                Ok(msg) => {
                    self.pool.put(msg);
                }
                Err(e) => {
                    tracing::warn!("{}:{}: malformed message, skipping: {}", path.display(), line_num + 1, e);
                    // Only skip if it's the last line (crash recovery).
                    // For interior lines, still warn but continue.
                }
            }
        }

        Ok(())
    }

    // Create a new session file and set it as active.
    pub fn new_session(&mut self, name: &str, cwd: &str) -> eyre::Result<PathBuf> {
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

        // Write header.
        let header = SessionHeader {
            session: name.to_string(),
            ts,
            cwd: Some(cwd.to_string()),
            extra: Default::default(),
        };
        let header_json = serde_json::to_string(&header)?;
        writeln!(file, "{}", header_json)?;

        // Generate prefix for message IDs.
        let prefix = gen_session_prefix(name);

        self.active = Some(ActiveSession {
            file,
            path: path.clone(),
            name: name.to_string(),
        });
        self.active_prefix = prefix;
        self.active_counter = 0;

        Ok(path)
    }

    // Generate a new message ID for the active session.
    pub fn next_id(&mut self) -> String {
        self.active_counter += 1;
        if self.active_prefix.is_empty() {
            gen_id()
        } else {
            format!("{}_{}", self.active_prefix, self.active_counter)
        }
    }

    // Write a message to the pool AND append to the active session file.
    pub fn write_message(&mut self, msg: Message) -> eyre::Result<String> {
        if msg.id.is_empty() {
            return Err(eyre::eyre!("Cannot write message with empty ID"));
        }
        let id = msg.id.clone();

        if let Some(ref mut session) = self.active {
            let json = serde_json::to_string(&msg)?;
            writeln!(session.file, "{}", json)?;
            session.file.flush()?;
        }

        self.pool.put(msg);
        Ok(id)
    }

    // List all sessions (from filenames + headers).
    pub fn list_sessions(&self) -> eyre::Result<Vec<SessionInfo>> {
        if !self.sessions_dir.exists() {
            return Ok(Vec::new());
        }

        let mut sessions = Vec::new();
        let mut entries: Vec<_> = fs::read_dir(&self.sessions_dir)?
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.path().extension()
                    .map_or(false, |ext| ext == "jsonl")
            })
            .collect();

        // Sort descending (newest first).
        entries.sort_by(|a, b| b.file_name().cmp(&a.file_name()));

        for entry in entries {
            let path = entry.path();
            match read_session_header(&path) {
                Ok(info) => sessions.push(info),
                Err(_) => continue,
            }
        }

        Ok(sessions)
    }

    pub fn active_session_name(&self) -> Option<&str> {
        self.active.as_ref().map(|s| s.name.as_str())
    }

    pub fn active_session_path(&self) -> Option<&Path> {
        self.active.as_ref().map(|s| s.path.as_path())
    }
}

fn read_session_header(path: &Path) -> eyre::Result<SessionInfo> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);

    for line in reader.lines() {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        // First non-empty line should be the header.
        // Detect structurally: has "session" key, no "role" key.
        if let Ok(obj) = serde_json::from_str::<serde_json::Value>(trimmed) {
            if obj.get("session").is_some() && obj.get("role").is_none() {
                if let Ok(header) = serde_json::from_str::<SessionHeader>(trimmed) {
                    return Ok(SessionInfo {
                        path: path.to_path_buf(),
                        name: header.session,
                        ts: header.ts,
                        cwd: header.cwd,
                    });
                }
            }
        }

        // Not a header, try to derive info from filename.
        break;
    }

    // Fallback: derive from filename.
    let stem = path.file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown");
    Ok(SessionInfo {
        path: path.to_path_buf(),
        name: stem.to_string(),
        ts: String::new(),
        cwd: None,
    })
}

// Generate a session prefix from name + timestamp.
// Format: <name_slug>_<time_component> to ensure uniqueness across sessions.
fn gen_session_prefix(name: &str) -> String {
    let slug: String = name.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(6)
        .collect();
    let ts = Utc::now().format("%H%M%S").to_string();
    if slug.is_empty() {
        format!("s{}", ts)
    } else {
        format!("{}{}", slug, ts)
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
    // Collapse multiple dashes.
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
