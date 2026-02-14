use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use chrono::Utc;
use serde::{Serialize, Deserialize};

use crate::JsonMap;
use crate::message::{gen_id, gen_session_prefix, ContentBlock, Message, MessagePool, Role};

/// Session header -- serialized as the first line of a .jsonl session file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionHeader {
    pub session: String,
    pub ts: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(flatten)]
    pub extra: JsonMap,
}

/// Manages the message pool and active session file. Loads history from
/// existing .jsonl files and writes new messages to the active session.
pub struct SessionStore {
    pub pool: MessagePool,
    sessions_dir: PathBuf,
    active: Option<File>,
    // Session prefix for generating message IDs in the active session.
    active_prefix: String,
    active_counter: u64,
}

impl SessionStore {
    pub fn new(sessions_dir: PathBuf) -> Self {
        SessionStore {
            pool: MessagePool::new(),
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

    /// Create a new filing, load history, start a session, and write the system message.
    /// Returns (filing, session_ids) ready for the agent loop.
    pub fn init(name: &str, cwd: &str, system_prompt: &str) -> eyre::Result<(Self, Vec<String>)> {
        let sessions_dir = Self::default_dir()?;
        let mut filing = Self::new(sessions_dir);
        filing.load_all()?;
        filing.new_session(name, cwd)?;

        let sys_id = filing.next_id();
        let sys_msg = Message::new(
            sys_id.clone(),
            Role::System,
            vec![ContentBlock::text(system_prompt)],
        );
        filing.write_message(sys_msg)?;

        Ok((filing, vec![sys_id]))
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
                }
            }
        }

        Ok(())
    }

    /// Create a new .jsonl session file and set it as active.
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

        self.active = Some(file);
        self.active_prefix = prefix;
        self.active_counter = 0;

        Ok(path)
    }

    /// Generate a new message ID for the active session.
    pub fn next_id(&mut self) -> String {
        self.active_counter += 1;
        if self.active_prefix.is_empty() {
            gen_id()
        } else {
            format!("{}_{}", self.active_prefix, self.active_counter)
        }
    }

    /// Write a message to the pool AND append to the active session file.
    pub fn write_message(&mut self, msg: Message) -> eyre::Result<String> {
        if msg.id.is_empty() {
            return Err(eyre::eyre!("Cannot write message with empty ID"));
        }
        let id = msg.id.clone();

        if let Some(ref mut file) = self.active {
            let json = serde_json::to_string(&msg)?;
            writeln!(file, "{}", json)?;
            file.flush()?;
        }

        self.pool.put(msg);
        Ok(id)
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
