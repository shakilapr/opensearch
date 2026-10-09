//! On-disk response cache.
//!
//! One file per `(base_url, model, prompt)` tuple, so re-running the organizer over unchanged
//! input never pays for the same completion twice.
//!
//! Layout: `cache_dir/<first two hex chars of key hash>/<full key hash>.json`, containing
//! `{"model": ..., "prompt_sha256": ..., "response": ...}`.
//!
//! Writes are **atomic**: the entry is written to a uniquely-named temp file in the same
//! directory and then `rename`d into place, so a concurrent reader (or a crash) can never
//! observe a half-written entry.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use crate::error::OrganizeError;

/// SHA-256 of the prompt alone, stored alongside the response for provenance.
pub fn prompt_sha256(prompt: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(prompt.as_bytes());
    hex::encode(h.finalize())
}

/// The cache key: `sha256(base_url \0 model \0 prompt)`, hex-encoded.
///
/// The `base_url` is part of the key so switching endpoints cannot return a response minted by
/// a different provider for the same prompt.
pub fn cache_key_hash(base_url: &str, model: &str, prompt: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(base_url.as_bytes());
    h.update([0u8]);
    h.update(model.as_bytes());
    h.update([0u8]);
    h.update(prompt.as_bytes());
    hex::encode(h.finalize())
}

#[derive(Serialize, Deserialize)]
struct CacheEntry {
    model: String,
    prompt_sha256: String,
    response: String,
}

/// Path of the cache entry for `hash` under `cache_dir`.
pub fn entry_path(cache_dir: &Path, hash: &str) -> PathBuf {
    // `hash` is always at least 2 chars (SHA-256 hex).
    cache_dir.join(&hash[0..2]).join(format!("{hash}.json"))
}

/// Read a cached response. Any read/parse failure is treated as a miss (`None`) so a corrupt
/// entry degrades to a fresh call rather than a hard error.
pub fn read_cache(cache_dir: &Path, hash: &str) -> Option<String> {
    let bytes = fs::read(entry_path(cache_dir, hash)).ok()?;
    let entry: CacheEntry = serde_json::from_slice(&bytes).ok()?;
    Some(entry.response)
}

/// Atomically write a cache entry.
pub fn write_cache_atomic(
    cache_dir: &Path,
    hash: &str,
    model: &str,
    prompt: &str,
    response: &str,
) -> Result<(), OrganizeError> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let dir = cache_dir.join(&hash[0..2]);
    fs::create_dir_all(&dir)?;

    let entry = CacheEntry {
        model: model.to_string(),
        prompt_sha256: prompt_sha256(prompt),
        response: response.to_string(),
    };
    let bytes = serde_json::to_vec(&entry)?;

    // Unique temp name in the SAME directory (rename is only atomic within a filesystem).
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp = dir.join(format!(".{hash}.{}.{}.tmp", std::process::id(), n));
    fs::write(&tmp, &bytes)?;
    fs::rename(&tmp, dir.join(format!("{hash}.json")))?;
    Ok(())
}

/// Count `*.json` cache entries under `cache_dir` (used by [`crate::CacheStats`]).
pub fn count_entries(cache_dir: &Path) -> u64 {
    let mut n = 0u64;
    if let Ok(subdirs) = fs::read_dir(cache_dir) {
        for sub in subdirs.flatten() {
            let p = sub.path();
            if !p.is_dir() {
                continue;
            }
            if let Ok(files) = fs::read_dir(&p) {
                for f in files.flatten() {
                    if f.path().extension().map(|x| x == "json").unwrap_or(false) {
                        n += 1;
                    }
                }
            }
        }
    }
    n
}
