//! On-disk answer cache.
//!
//! Layout mirrors `search-organize`: `cache_dir/<first two hex>/<full key hash>.json`, containing
//! `{"model": ..., "prompt_version": ..., "prompt_sha256": ..., "answer": ...}`.
//!
//! * The key is `sha256(model \0 prompt_version \0 prompt)`, so a prompt-version bump (or a model
//!   swap) can never return a stale answer minted under different instructions.
//! * Writes are **atomic**: bytes go to a uniquely-named temp file in the same directory and are
//!   then `rename`d into place, so a reader (or a crash) never observes a half-written entry. A
//!   partial write can therefore never poison the cache.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use crate::error::AnswerError;

/// SHA-256 of the prompt alone, stored alongside the answer for provenance.
pub fn prompt_sha256(prompt: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(prompt.as_bytes());
    hex::encode(h.finalize())
}

/// The cache key: `sha256(model \0 prompt_version \0 prompt)`, hex-encoded.
pub fn cache_key_hash(model: &str, prompt_version: &str, prompt: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(model.as_bytes());
    h.update([0u8]);
    h.update(prompt_version.as_bytes());
    h.update([0u8]);
    h.update(prompt.as_bytes());
    hex::encode(h.finalize())
}

#[derive(Serialize, Deserialize)]
struct CacheEntry {
    model: String,
    prompt_version: String,
    prompt_sha256: String,
    answer: String,
}

/// Path of the cache entry for `hash` under `cache_dir`.
pub fn entry_path(cache_dir: &Path, hash: &str) -> PathBuf {
    // `hash` is always at least 2 chars (SHA-256 hex).
    cache_dir.join(&hash[0..2]).join(format!("{hash}.json"))
}

/// Read a cached answer. Any read/parse failure is treated as a miss (`None`) so a corrupt entry
/// degrades to a fresh call rather than a hard error.
pub fn read_cache(cache_dir: &Path, hash: &str) -> Option<String> {
    let bytes = fs::read(entry_path(cache_dir, hash)).ok()?;
    let entry: CacheEntry = serde_json::from_slice(&bytes).ok()?;
    Some(entry.answer)
}

/// Build the on-disk bytes for one cache entry.
fn cache_bytes(model: &str, prompt_version: &str, prompt: &str, answer: &str) -> Vec<u8> {
    let entry = CacheEntry {
        model: model.to_string(),
        prompt_version: prompt_version.to_string(),
        prompt_sha256: prompt_sha256(prompt),
        answer: answer.to_string(),
    };
    // `serde_json::to_vec` on this struct cannot fail; fall back to an empty vec defensively.
    serde_json::to_vec(&entry).unwrap_or_default()
}

/// Write the temp file for `hash` without renaming it into place.
///
/// Exposed as a **test seam** so a hermetic test can simulate a crash between writing the temp
/// file and the atomic `rename`, then assert the partial temp never becomes visible via
/// [`read_cache`]. Production [`write_cache_atomic`] always renames.
pub fn write_cache_temp_only(cache_dir: &Path, hash: &str, bytes: &[u8]) -> std::io::Result<PathBuf> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let dir = cache_dir.join(&hash[0..2]);
    fs::create_dir_all(&dir)?;
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp = dir.join(format!(".{hash}.{}.{}.tmp", std::process::id(), n));
    fs::write(&tmp, bytes)?;
    Ok(tmp)
}

/// Atomically write a cache entry: temp file in the same directory, then `rename`.
pub fn write_cache_atomic(
    cache_dir: &Path,
    hash: &str,
    model: &str,
    prompt_version: &str,
    prompt: &str,
    answer: &str,
) -> Result<(), AnswerError> {
    let bytes = cache_bytes(model, prompt_version, prompt, answer);
    let tmp = write_cache_temp_only(cache_dir, hash, &bytes)?;
    fs::rename(&tmp, entry_path(cache_dir, hash))?;
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
