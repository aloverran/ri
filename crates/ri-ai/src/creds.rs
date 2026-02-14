// Shared credential storage for OAuth providers.

use std::path::{Path, PathBuf};

pub fn epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

pub fn ri_dir() -> eyre::Result<PathBuf> {
    ri::home_dir()
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct Credentials {
    pub access_token: String,
    pub refresh_token: String,
    pub expires: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
}

/// Buffer subtracted from expiry so we refresh before the token actually expires.
const EXPIRY_BUFFER_MS: u64 = 5 * 60 * 1000;

impl Credentials {
    pub fn is_expired(&self) -> bool {
        epoch_ms() >= self.expires
    }

    /// Compute an expiry timestamp from an expires_in duration in seconds.
    /// Subtracts a 5-minute buffer to ensure we refresh before actual expiry.
    pub fn compute_expiry(expires_in_seconds: u64) -> u64 {
        epoch_ms() + (expires_in_seconds * 1000).saturating_sub(EXPIRY_BUFFER_MS)
    }
}

pub fn load(path: &Path) -> Option<Credentials> {
    let data = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&data).ok()
}

pub fn save(path: &Path, creds: &Credentials) -> eyre::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let data = serde_json::to_string_pretty(creds)?;
    std::fs::write(path, &data)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}
