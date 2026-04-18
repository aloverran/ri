// Shared credential storage for OAuth providers.
//
// The core concern here is refresh coordination. Three OAuth providers
// (Anthropic, Google, OpenAI Codex) share the same `~/.ri/<provider>_auth.json`
// file between `ri-cli` and `ri-web`, which run as separate processes and
// may each spawn several tokio tasks that race on token expiry. OAuth
// providers rotate refresh tokens single-use (RFC 9700), so the loser of a
// concurrent refresh gets `invalid_grant` and -- if it also invalidates the
// token family -- forces a full re-login.
//
// The fix is an OS-level advisory lock via a sidecar `<creds>.json.lock` file.
// `flock`/`LockFileEx` serialize across processes *and* across tokio tasks
// within one process (the kernel queues on the open-file-description), so a
// single primitive covers both axes. The lock is never held across anything
// but the refresh-critical section (HTTP + save).

use std::io::Write;
use std::path::{Path, PathBuf};
use base64::Engine;
use fs4::fs_std::FileExt;
use rand::RngCore;
use sha2::{Digest, Sha256};

pub fn epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

pub fn ri_dir() -> eyre::Result<PathBuf> {
    let home = dirs::home_dir()
        .ok_or_else(|| eyre::eyre!("Could not determine home directory"))?;
    Ok(home.join(".ri"))
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

    /// Compute an expiry timestamp from an `expires_in` duration in seconds.
    /// Subtracts a 5-minute buffer so we refresh before the provider actually
    /// rejects the token.
    pub fn compute_expiry(expires_in_seconds: u64) -> u64 {
        epoch_ms() + (expires_in_seconds * 1000).saturating_sub(EXPIRY_BUFFER_MS)
    }
}

pub fn load(path: &Path) -> Option<Credentials> {
    let data = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&data).ok()
}

/// Save credentials to disk via atomic tempfile-and-rename. Callers that race
/// with a concurrent refresh should hold a `CredsGuard` -- `save` itself does
/// not serialize writers, it only makes the swap atomic relative to readers.
pub fn save(path: &Path, creds: &Credentials) -> eyre::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let data = serde_json::to_string_pretty(creds)?;
    atomic_write(path, data.as_bytes())?;
    Ok(())
}

/// Write `bytes` to `path` such that concurrent readers see either the
/// previous contents or the new contents, never a torn write. Works by
/// writing to a uniquely-named sibling tempfile and renaming on top.
///
/// The tempfile name is randomized per-call so concurrent writers that
/// bypass the credential lock (e.g. `complete_login`, which runs one at
/// a time by construction) do not stomp on each other's tempfiles -- a
/// safety net, not the primary guard against refresh races.
///
/// File permissions are locked down on the tempfile *before* the rename,
/// so there is no window where fresh creds sit at their final path with
/// the default (world-readable) umask.
fn atomic_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp_name = format!(
        "{}.tmp.{:x}.{:x}",
        path.file_name().and_then(|s| s.to_str()).unwrap_or("auth"),
        std::process::id(),
        rand::random::<u64>()
    );
    let tmp = path.with_file_name(tmp_name);
    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    // Apply restrictive permissions before the rename so the published
    // file is never world-readable, even momentarily.
    if let Err(e) = restrict_file_permissions(&tmp) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    // On failure we'd leak the tempfile; clean up explicitly.
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    // Best-effort fsync the parent directory so the rename survives power
    // loss. Directory fsync is a no-op on Windows (which has no dir fd
    // abstraction), so failures there are ignored.
    if let Some(parent) = path.parent() {
        if let Ok(dir) = std::fs::File::open(parent) {
            let _ = dir.sync_all();
        }
    }
    Ok(())
}

#[cfg(unix)]
pub fn restrict_file_permissions(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
pub fn restrict_file_permissions(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

// -- Refresh coordination --

/// Exclusive hold on the OS advisory lock for a credentials file. The lock
/// is owned by an open file descriptor on a sidecar `<creds>.json.lock`
/// file; dropping the guard closes the fd and releases the lock (including
/// on panic or process exit, since FDs are OS-reaped). Readers that don't
/// need to refresh may skip the guard entirely -- the lock only serializes
/// writers, readers see valid (old or new) JSON via atomic rename.
///
/// Use the lock sparingly: only around `refresh_token` and the `write` that
/// follows it. Holding it during long HTTP waits blocks every other refresher.
pub struct CredsGuard {
    data_path: PathBuf,
    // The file handle owns the flock. Kept alive for the guard's lifetime.
    _lock_file: std::fs::File,
}

impl CredsGuard {
    /// Acquire the exclusive refresh lock for the credentials file at
    /// `data_path`. Blocks until the lock is available; the kernel queues
    /// waiters both across processes and across tokio tasks within one
    /// process. Lock acquisition runs on the tokio blocking pool so the
    /// async runtime stays responsive while waiting.
    pub async fn acquire(data_path: impl Into<PathBuf>) -> eyre::Result<Self> {
        let data_path = data_path.into();
        let lock_path = lock_path_for(&data_path);

        let _lock_file = tokio::task::spawn_blocking(move || -> eyre::Result<std::fs::File> {
            if let Some(parent) = lock_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            // `write` is required for Windows LockFileEx exclusive mode.
            // `create` so first-run users don't need a pre-existing lockfile.
            let f = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&lock_path)?;
            FileExt::lock_exclusive(&f)?;
            Ok(f)
        })
        .await??;

        Ok(Self { data_path, _lock_file })
    }

    /// Read the credentials file under the guard's lock. Returns `None` if
    /// the file is absent or unparseable; absence is the expected first-run
    /// state, parse errors surface as `None` because callers treat both as
    /// "need fresh auth".
    pub fn read(&self) -> Option<Credentials> {
        load(&self.data_path)
    }

    /// Persist `creds` under the guard's lock via atomic rename. Use this
    /// instead of `creds::save` when you are the holder of a refresh lock,
    /// to keep the read-modify-write window serialized. Disk I/O runs on
    /// the tokio blocking pool because `fsync` can stall the calling thread
    /// for tens of milliseconds on a busy disk.
    pub async fn write(&self, creds: &Credentials) -> eyre::Result<()> {
        let path = self.data_path.clone();
        let creds = creds.clone();
        tokio::task::spawn_blocking(move || save(&path, &creds)).await?
    }
}

fn lock_path_for(data_path: &Path) -> PathBuf {
    let mut s = data_path.as_os_str().to_owned();
    s.push(".lock");
    PathBuf::from(s)
}

// -- PKCE --

pub fn generate_verifier() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

pub fn challenge(verifier: &str) -> String {
    let hash = Sha256::digest(verifier.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(hash)
}
