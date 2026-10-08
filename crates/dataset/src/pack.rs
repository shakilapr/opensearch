//! Release packs: JSONL shards + a checksummed manifest, and the write/validate round-trip.

use std::io::Write;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::checksum::checksum_hex;
use crate::{DatasetAdapter, DatasetError, Record, PACK_SCHEMA};

/// Manifest file name inside a pack directory.
pub const MANIFEST_FILE: &str = "manifest.json";

/// One shard of a pack: a JSONL file plus its record count and checksum.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShardInfo {
    /// Path relative to the pack directory.
    pub path: String,
    pub records: u64,
    /// Checksum of the shard's raw bytes (see [`crate::checksum`]).
    pub checksum: String,
}

/// A pack's manifest, written next to its shards.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackManifest {
    pub name: String,
    pub version: String,
    pub records: u64,
    pub shards: Vec<ShardInfo>,
    pub created_at: i64,
    pub schema: u32,
}

/// Write `records` as JSONL shards of at most `shard_size` records each, plus a manifest.
///
/// Shards are named `{name}-{index:05}.jsonl`; the manifest is `manifest.json`. The manifest's
/// `records` is the total across shards and each [`ShardInfo`] carries the checksum of the
/// shard's exact on-disk bytes, so [`validate_pack`] can detect any later tampering.
pub fn write_pack(
    dir: &Path,
    name: &str,
    version: &str,
    records: impl IntoIterator<Item = Record>,
    shard_size: usize,
) -> Result<PackManifest, DatasetError> {
    if shard_size == 0 {
        return Err(DatasetError::Parse("shard_size must be greater than zero".into()));
    }
    if name.trim().is_empty() {
        return Err(DatasetError::Parse("pack name must not be empty".into()));
    }

    std::fs::create_dir_all(dir)?;

    let mut shards: Vec<ShardInfo> = Vec::new();
    let mut total: u64 = 0;
    let mut index: usize = 0;
    let mut iter = records.into_iter();

    loop {
        let mut buf: Vec<Record> = Vec::with_capacity(shard_size.min(1024));
        while buf.len() < shard_size {
            match iter.next() {
                Some(r) => buf.push(r),
                None => break,
            }
        }
        if buf.is_empty() {
            break;
        }

        let mut bytes: Vec<u8> = Vec::new();
        for record in &buf {
            serde_json::to_writer(&mut bytes, record)?;
            bytes.push(b'\n');
        }

        let path = format!("{name}-{index:05}.jsonl");
        let mut file = std::fs::File::create(dir.join(&path))?;
        file.write_all(&bytes)?;
        file.sync_all()?;

        shards.push(ShardInfo {
            path,
            records: buf.len() as u64,
            checksum: checksum_hex(&bytes),
        });
        total += buf.len() as u64;
        index += 1;
    }

    let manifest = PackManifest {
        name: name.to_string(),
        version: version.to_string(),
        records: total,
        shards,
        created_at: now_epoch(),
        schema: PACK_SCHEMA,
    };
    let manifest_bytes = serde_json::to_vec_pretty(&manifest)?;
    std::fs::write(dir.join(MANIFEST_FILE), manifest_bytes)?;

    Ok(manifest)
}

/// Re-read + verify a pack on disk (shard checksums + record counts match the manifest).
///
/// Fails if the manifest is missing/unreadable, any shard is missing, a shard's checksum no
/// longer matches, a shard's JSONL line count differs from the manifest, or the per-shard
/// counts do not sum to the manifest total.
pub fn validate_pack(dir: &Path) -> Result<PackManifest, DatasetError> {
    let manifest_bytes = std::fs::read(dir.join(MANIFEST_FILE))?;
    let manifest: PackManifest = serde_json::from_slice(&manifest_bytes)?;

    let mut total: u64 = 0;
    for shard in &manifest.shards {
        let bytes = std::fs::read(dir.join(&shard.path))?;
        let actual = checksum_hex(&bytes);
        if actual != shard.checksum {
            return Err(DatasetError::Parse(format!(
                "checksum mismatch for shard {}: manifest {} != on-disk {}",
                shard.path, shard.checksum, actual
            )));
        }
        let count = count_jsonl_records(&bytes, &shard.path)?;
        if count != shard.records {
            return Err(DatasetError::Parse(format!(
                "record count mismatch for shard {}: manifest {} != on-disk {}",
                shard.path, shard.records, count
            )));
        }
        total += count;
    }

    if total != manifest.records {
        return Err(DatasetError::Parse(format!(
            "total record count mismatch: manifest {} != shards {}",
            manifest.records, total
        )));
    }

    Ok(manifest)
}

/// End-to-end: pull from an adapter until exhausted (bounded by `max_records`), write a pack.
///
/// Paging follows [`Page::next`] cursors; [`DatasetError::Exhausted`] ends the pull cleanly.
/// `max_records` caps the number of records written (a value of `0` writes an empty pack).
pub fn build_from_adapter(
    adapter: &mut dyn DatasetAdapter,
    dir: &Path,
    name: &str,
    version: &str,
    shard_size: usize,
    max_records: u64,
) -> Result<PackManifest, DatasetError> {
    let mut records: Vec<Record> = Vec::new();
    let mut cursor: Option<String> = None;

    loop {
        if records.len() as u64 >= max_records {
            break;
        }
        match adapter.page(cursor.as_deref()) {
            Ok(page) => {
                for record in page.records {
                    if records.len() as u64 >= max_records {
                        break;
                    }
                    records.push(record);
                }
                match page.next {
                    Some(token) => cursor = Some(token),
                    None => break,
                }
            }
            Err(DatasetError::Exhausted) => break,
            Err(e) => return Err(e),
        }
    }

    write_pack(dir, name, version, records, shard_size)
}

/// Count well-formed JSON lines in a shard (trailing/blank lines ignored).
fn count_jsonl_records(bytes: &[u8], path: &str) -> Result<u64, DatasetError> {
    let mut count = 0u64;
    for (i, line) in bytes.split(|b| *b == b'\n').enumerate() {
        if line.iter().all(|b| b.is_ascii_whitespace()) {
            continue;
        }
        serde_json::from_slice::<Record>(line).map_err(|e| {
            DatasetError::Parse(format!("shard {path} line {}: {e}", i + 1))
        })?;
        count += 1;
    }
    Ok(count)
}

fn now_epoch() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
