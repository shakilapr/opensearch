//! Release packs: JSONL shards + a checksummed manifest, and the write/validate round-trip.

use std::collections::HashSet;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

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

/// Read every record from a pack directory, verifying it against its manifest first.
///
/// Shards are returned in manifest order, records in file order. This is the convenience
/// entry point for consumers that want the whole pack in memory; it delegates to
/// [`PackReader`], which validates the pack with [`validate_pack`] before yielding anything.
pub fn read_pack(dir: &Path) -> Result<Vec<Record>, DatasetError> {
    PackReader::open(dir)?.collect()
}

/// Streaming reader over a validated pack: shard by shard, without holding the whole pack in
/// memory.
///
/// [`open`](PackReader::open) verifies the pack with [`validate_pack`] (checksums, record
/// counts and manifest totals) and only then yields records. Records arrive in manifest shard
/// order then file order; a malformed JSON line surfaces as [`DatasetError::Parse`] naming the
/// shard and its 1-based line number, never a panic. Implements [`Iterator`], so `collect`,
/// `map`, `take` and friends work directly.
pub struct PackReader {
    dir: PathBuf,
    manifest: PackManifest,
    /// Index of the next shard to open; shards before it are fully consumed.
    shard_index: usize,
    reader: Option<BufReader<std::fs::File>>,
    /// Path of the shard currently being read (relative to `dir`), for error messages.
    current_path: String,
    /// 1-based number of the line last read from the current shard.
    line_no: usize,
}

impl PackReader {
    /// Open `dir`, validate it against its manifest, and prepare to stream its records.
    pub fn open(dir: &Path) -> Result<Self, DatasetError> {
        let manifest = validate_pack(dir)?;
        Ok(PackReader {
            dir: dir.to_path_buf(),
            manifest,
            shard_index: 0,
            reader: None,
            current_path: String::new(),
            line_no: 0,
        })
    }

    /// The manifest's record count (the number of records the reader will yield).
    pub fn total(&self) -> u64 {
        self.manifest.records
    }

    /// The validated manifest this reader was opened with.
    pub fn manifest(&self) -> &PackManifest {
        &self.manifest
    }

    /// Pull the next record, or `None` once every shard is exhausted.
    pub fn next_record(&mut self) -> Result<Option<Record>, DatasetError> {
        loop {
            if self.reader.is_none() {
                if self.shard_index >= self.manifest.shards.len() {
                    return Ok(None);
                }
                self.current_path = self.manifest.shards[self.shard_index].path.clone();
                self.shard_index += 1;
                let file = std::fs::File::open(self.dir.join(&self.current_path))?;
                self.reader = Some(BufReader::new(file));
                self.line_no = 0;
            }

            let mut line = String::new();
            let n = self
                .reader
                .as_mut()
                .expect("reader is open inside the loop")
                .read_line(&mut line)?;
            if n == 0 {
                self.reader = None;
                continue;
            }
            self.line_no += 1;
            if line.trim().is_empty() {
                continue;
            }
            let record = serde_json::from_str::<Record>(&line).map_err(|e| {
                DatasetError::Parse(format!(
                    "shard {} line {}: {e}",
                    self.current_path, self.line_no
                ))
            })?;
            return Ok(Some(record));
        }
    }
}

impl Iterator for PackReader {
    type Item = Result<Record, DatasetError>;

    fn next(&mut self) -> Option<Self::Item> {
        self.next_record().transpose()
    }
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

/// An **incremental, resumable** pack writer.
///
/// [`write_pack`] takes the whole record set and is all-or-nothing: an export that organises
/// 87 000 pages through a slow model and is then killed (a `timeout`, a reboot) leaves **nothing**
/// on disk, so every re-run starts from zero and a long export can never finish. This writer
/// closes records to disk as they arrive — a shard per `shard_size` records, with the manifest
/// rewritten atomically after each one — so an interrupted run keeps everything written so far and
/// the next run continues it.
///
/// Resume is by content, not by count: [`existing_ids`](PackWriter::existing_ids) returns the
/// record ids already on disk so the caller can skip pages it has already exported. The on-disk
/// shape is exactly the one [`validate_pack`] checks (shards + checksummed manifest), so a pack
/// written incrementally is a normal pack.
pub struct PackWriter {
    dir: PathBuf,
    name: String,
    version: String,
    shard_size: usize,
    shards: Vec<ShardInfo>,
    total: u64,
    buf: Vec<Record>,
}

impl PackWriter {
    /// Open a pack directory for incremental writing, **resuming** one that already exists.
    ///
    /// With no `manifest.json` this starts a fresh pack. With one, the pack is validated first and
    /// its shards are adopted, so new shards continue the same pack instead of overwriting it. A
    /// manifest that does **not** validate is an error rather than a silent restart: starting over
    /// would leave stale shards behind and could collide with their file names.
    pub fn open(
        dir: &Path,
        name: &str,
        version: &str,
        shard_size: usize,
    ) -> Result<Self, DatasetError> {
        if shard_size == 0 {
            return Err(DatasetError::Parse("shard_size must be greater than zero".into()));
        }
        if name.trim().is_empty() {
            return Err(DatasetError::Parse("pack name must not be empty".into()));
        }
        std::fs::create_dir_all(dir)?;

        let (shards, total) = if dir.join(MANIFEST_FILE).exists() {
            let existing = validate_pack(dir)?;
            (existing.shards, existing.records)
        } else {
            (Vec::new(), 0)
        };

        Ok(Self {
            dir: dir.to_path_buf(),
            name: name.to_string(),
            version: version.to_string(),
            shard_size,
            shards,
            total,
            buf: Vec::new(),
        })
    }

    /// The record ids already present in the pack — the resume key set.
    ///
    /// Read from the shards themselves (after validating the pack), so a multi-record page and a
    /// single-record page resume identically and no separate bookkeeping file can drift.
    pub fn existing_ids(&self) -> Result<HashSet<String>, DatasetError> {
        let mut ids = HashSet::new();
        for shard in &self.shards {
            let file = BufReader::new(std::fs::File::open(self.dir.join(&shard.path))?);
            for line in file.lines() {
                let line = line?;
                if line.trim().is_empty() {
                    continue;
                }
                let record: Record = serde_json::from_str(&line)?;
                ids.insert(record.id);
            }
        }
        Ok(ids)
    }

    /// How many records are durably on disk.
    pub fn records(&self) -> u64 {
        self.total
    }

    /// Buffer a record. A full shard is written (and the manifest updated) immediately, so at most
    /// `shard_size - 1` records are ever at risk.
    pub fn push(&mut self, record: Record) -> Result<(), DatasetError> {
        self.buf.push(record);
        if self.buf.len() >= self.shard_size {
            self.write_shard()?;
        }
        Ok(())
    }

    /// Write the buffered records as a shard and rewrite the manifest atomically. Cheap to call
    /// often — call it at every natural checkpoint (a chunk of pages) so a kill loses one chunk.
    pub fn flush(&mut self) -> Result<PackManifest, DatasetError> {
        if !self.buf.is_empty() {
            self.write_shard()?;
        }
        self.write_manifest()?;
        Ok(self.manifest())
    }

    /// Flush and return the final manifest.
    pub fn finish(mut self) -> Result<PackManifest, DatasetError> {
        self.flush()?;
        Ok(self.manifest())
    }

    /// The manifest as it would be written right now.
    pub fn manifest(&self) -> PackManifest {
        PackManifest {
            name: self.name.clone(),
            version: self.version.clone(),
            records: self.total,
            shards: self.shards.clone(),
            created_at: now_epoch(),
            schema: PACK_SCHEMA,
        }
    }

    fn write_shard(&mut self) -> Result<(), DatasetError> {
        let mut bytes: Vec<u8> = Vec::new();
        for record in &self.buf {
            serde_json::to_writer(&mut bytes, record)?;
            bytes.push(b'\n');
        }

        let path = format!("{}-{:05}.jsonl", self.name, self.shards.len());
        let mut file = std::fs::File::create(self.dir.join(&path))?;
        file.write_all(&bytes)?;
        file.sync_all()?;

        self.shards.push(ShardInfo {
            path,
            records: self.buf.len() as u64,
            checksum: checksum_hex(&bytes),
        });
        self.total += self.buf.len() as u64;
        self.buf.clear();
        // The shard must be in the manifest as soon as it exists: an unlisted shard would be
        // skipped by `validate_pack` and then overwritten by the next run's shard of the same
        // index.
        self.write_manifest()
    }

    /// Write the manifest through a temp file + rename so a kill mid-write cannot leave a
    /// truncated manifest (which would make the pack unresumable).
    fn write_manifest(&self) -> Result<(), DatasetError> {
        let bytes = serde_json::to_vec_pretty(&self.manifest())?;
        let tmp = self.dir.join(format!("{MANIFEST_FILE}.tmp"));
        std::fs::write(&tmp, &bytes)?;
        std::fs::rename(&tmp, self.dir.join(MANIFEST_FILE))?;
        Ok(())
    }
}
