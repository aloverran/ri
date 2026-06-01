//! Content-addressed blob store: the global place bytes live.
//!
//! A binary blob (an image, an audio clip, a PDF, any file) does not
//! belong in the `Pool`. The pool is RAM-resident and re-parsed every
//! mount; duplicating arbitrarily large blobs there would be ruinous. So
//! bytes live in **one global content-addressed store the pool never
//! loads**: a flat directory of immutable files at `~/.ri/blobs/<hash>`.
//!
//! The whole design rides on one decision: **the hash *is* the filename,
//! so the filesystem is the index.** There is no in-memory map, no
//! per-family bookkeeping -- `get(hash)` is a single `read` of one file,
//! `contains` is an `exists`, dedup is automatic (identical bytes hash to
//! the same name, so they are one file forever). A `ContentBlock::Blob`
//! carries only the pure content address `{media_type, hash, name?, size}`
//! and never the bytes.
//!
//! This module is a **leaf**: it knows nothing of messages, contexts, or
//! families. It hashes bytes, writes them atomically, hands them back, and
//! (on demand) sweeps anything unreferenced. The reachable-set *driver*
//! that walks contexts to feed [`Blobs::gc`] lives elsewhere; this
//! primitive is total and self-contained.

use std::borrow::Borrow;
use std::collections::HashSet;
use std::fmt;
use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

/// The address of a blob: the lowercase sha256 hex of its bytes.
///
/// A blob hash is *content-derived*, never minted -- there is deliberately
/// no `generate()`. Two byte-identical blobs always carry the same hash,
/// which is the whole point: the address is the dedup key and the
/// filename in one.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct BlobHash(String);

impl BlobHash {
    pub fn new(s: impl Into<String>) -> Self { Self(s.into()) }
    pub fn as_str(&self) -> &str { &self.0 }
}

impl fmt::Display for BlobHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str(&self.0) }
}

impl AsRef<str> for BlobHash {
    fn as_ref(&self) -> &str { &self.0 }
}

impl Borrow<str> for BlobHash {
    fn borrow(&self) -> &str { &self.0 }
}

impl From<String> for BlobHash {
    fn from(s: String) -> Self { Self(s) }
}

impl From<&str> for BlobHash {
    fn from(s: &str) -> Self { Self(s.to_string()) }
}

impl From<&String> for BlobHash {
    fn from(s: &String) -> Self { Self(s.clone()) }
}

/// A cheap handle on the global content-addressed store rooted at one
/// directory. Clone freely -- it carries only a path; the filesystem is
/// the real state.
#[derive(Debug, Clone)]
pub struct Blobs {
    root: PathBuf,
}

impl Blobs {
    /// Open (creating if needed) a blob store rooted at `root`.
    pub fn new(root: impl Into<PathBuf>) -> std::io::Result<Self> {
        let root = root.into();
        fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    /// Store bytes and return their address. Idempotent: if a blob with
    /// these exact bytes already exists, the existing file is left
    /// untouched and its hash returned without a rewrite.
    ///
    /// The write is atomic. Bytes go to a unique temp path (`.tmp-<uuid>`,
    /// the nonce guarding against a concurrent writer clobbering the same
    /// temp) and are then `rename`d onto the final path -- atomic on the
    /// same directory, so a crash mid-write can never leave a half-blob
    /// masquerading as a real one. On a rename failure the temp is cleaned
    /// up best-effort.
    pub fn put(&self, bytes: &[u8]) -> std::io::Result<BlobHash> {
        let hash = hash_bytes(bytes);
        let final_path = self.root.join(hash.as_str());

        // Already present: identical bytes => identical hash. Nothing to do.
        if final_path.exists() {
            return Ok(hash);
        }

        let tmp_path = self.root.join(format!(".tmp-{}", Uuid::new_v4().simple()));
        fs::write(&tmp_path, bytes)?;
        match fs::rename(&tmp_path, &final_path) {
            Ok(()) => Ok(hash),
            Err(e) => {
                // Best-effort cleanup; the rename failure is the real error.
                let _ = fs::remove_file(&tmp_path);
                Err(e)
            }
        }
    }

    /// Read a blob's bytes. A missing blob is `Ok(None)` (a dangling hash
    /// is non-fatal -- the same contract a missing message id has), not an
    /// error.
    pub fn get(&self, hash: &BlobHash) -> std::io::Result<Option<Vec<u8>>> {
        match fs::read(self.path(hash)) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// The size of a blob in bytes without reading it, or `None` if it is
    /// missing. Lets a placeholder render with zero I/O.
    pub fn stat(&self, hash: &BlobHash) -> Option<u64> {
        fs::metadata(self.path(hash)).ok().map(|m| m.len())
    }

    /// Whether a blob is present.
    pub fn contains(&self, hash: &BlobHash) -> bool {
        self.path(hash).exists()
    }

    /// The on-disk path a blob lives at (`<root>/<hash>`).
    pub fn path(&self, hash: &BlobHash) -> PathBuf {
        self.root.join(hash.as_str())
    }

    /// Mark-and-sweep: delete every blob whose hash is **not** in
    /// `reachable`, returning the number of files removed.
    ///
    /// The enclosing directory and any in-flight `.tmp-*` files are
    /// skipped (a temp belongs to a `put` racing this sweep; never a blob).
    /// The reachable set is computed by the caller, which walks every
    /// loaded context -- that driver is deliberately not this leaf's job.
    pub fn gc(&self, reachable: &HashSet<BlobHash>) -> std::io::Result<usize> {
        let mut deleted = 0;
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if name.starts_with(".tmp-") {
                continue;
            }
            if !reachable.contains(name) {
                fs::remove_file(entry.path())?;
                deleted += 1;
            }
        }
        Ok(deleted)
    }
}

/// Hash bytes to a lowercase sha256 hex `BlobHash`.
fn hash_bytes(bytes: &[u8]) -> BlobHash {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    BlobHash(format!("{:x}", hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_root(tag: &str) -> PathBuf {
        let unique = Uuid::new_v4().simple().to_string();
        std::env::temp_dir().join(format!("ri-blobs-{}-{}", tag, unique))
    }

    #[test]
    fn roundtrip_dedup_and_gc() {
        let root = tmp_root("rt");
        let blobs = Blobs::new(&root).expect("open store");

        // Round-trip: bytes go in, the same bytes come out.
        let bytes = b"the bytes do not lie; extensions do".to_vec();
        let hash = blobs.put(&bytes).expect("put");
        assert!(blobs.contains(&hash));
        assert_eq!(blobs.get(&hash).expect("get"), Some(bytes.clone()));
        assert_eq!(blobs.stat(&hash), Some(bytes.len() as u64));

        // Dedup: identical bytes => identical hash => one file, idempotent.
        let hash2 = blobs.put(&bytes).expect("put again");
        assert_eq!(hash, hash2);
        let on_disk = fs::read_dir(&root).unwrap().filter(|e| {
            e.as_ref().unwrap().file_type().unwrap().is_file()
        }).count();
        assert_eq!(on_disk, 1, "dedup should leave exactly one file");

        // A distinct blob, plus a miss on an unknown hash.
        let other = blobs.put(b"a different file entirely").expect("put other");
        assert_ne!(hash, other);
        let unknown = BlobHash::new("0".repeat(64));
        assert_eq!(blobs.get(&unknown).expect("get unknown"), None);
        assert_eq!(blobs.stat(&unknown), None);
        assert!(!blobs.contains(&unknown));

        // gc: keep `hash`, sweep `other`.
        let mut reachable = HashSet::new();
        reachable.insert(hash.clone());
        let swept = blobs.gc(&reachable).expect("gc");
        assert_eq!(swept, 1);
        assert!(blobs.contains(&hash));
        assert!(!blobs.contains(&other));

        let _ = fs::remove_dir_all(&root);
    }
}
