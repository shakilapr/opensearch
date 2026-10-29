//! A document-vector sidecar: `doc_id` → embedding, with cosine top-k.
//!
//! Why a sidecar rather than an index field: the Tantivy schema is frozen (its hash is asserted
//! on open, `crates/indexer/src/store.rs`), so adding a vector field would invalidate every
//! existing index and force a rebuild. Vectors are written by a separate pass that only needs
//! **read** access to the index, which is why the backfill can run while a crawl holds the
//! writer lock — the same reason `serve` can now open read-only.
//!
//! The format is deliberately trivial: a magic header, a dimension, a count, then
//! length-prefixed ids and `f32` values. It is a build artifact (regenerable from the index), so
//! it carries no compatibility promise beyond rejecting anything that is not its own magic.
//! Vectors are stored L2-normalised by their producer, but `top_k` normalises defensively, so a
//! store written by a future producer with a different convention still ranks by cosine.

use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::Path;

/// Magic header. A file that does not start with this is not a vector store.
const MAGIC: &[u8; 8] = b"ANSEVEC1";

/// Errors from reading or writing a vector store.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// The file could not be read or written.
    #[error("vector store I/O: {0}")]
    Io(#[from] std::io::Error),
    /// The file is not a vector store, or was written by an incompatible version.
    #[error("vector store format: {0}")]
    Format(String),
    /// A vector did not match the store's dimension.
    #[error("dimension mismatch: store is {expected}-dimensional but got {got}")]
    Dim {
        /// The store's dimension.
        expected: usize,
        /// The offending vector's length.
        got: usize,
    },
}

/// One stored document vector.
#[derive(Debug, Clone, PartialEq)]
pub struct VectorEntry {
    /// Document identity (the Tantivy delete term).
    pub doc_id: String,
    /// Owning source, when the document has one. Stored so the lane can honour a
    /// `source_id`-scoped call exactly, instead of returning unscoped hits.
    pub source_id: Option<String>,
    /// The vector (L2-normalised by its producer).
    pub v: Vec<f32>,
}

/// A collection of document vectors of one fixed dimension.
#[derive(Debug, Clone, PartialEq)]
pub struct VectorStore {
    dim: usize,
    entries: Vec<VectorEntry>,
}

impl VectorStore {
    /// An empty store for `dim`-dimensional vectors.
    pub fn new(dim: usize) -> Self {
        Self { dim, entries: Vec::new() }
    }

    /// The dimension every vector in this store must have.
    pub fn dim(&self) -> usize {
        self.dim
    }

    /// Number of documents.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the store holds no documents.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Insert or replace one document vector.
    pub fn insert(
        &mut self,
        doc_id: impl Into<String>,
        source_id: Option<String>,
        v: Vec<f32>,
    ) -> Result<(), StoreError> {
        if v.len() != self.dim {
            return Err(StoreError::Dim { expected: self.dim, got: v.len() });
        }
        let doc_id = doc_id.into();
        match self.entries.iter_mut().find(|e| e.doc_id == doc_id) {
            Some(slot) => {
                slot.v = v;
                slot.source_id = source_id;
            }
            None => self.entries.push(VectorEntry { doc_id, source_id, v }),
        }
        Ok(())
    }

    /// The vector for one document, if present.
    pub fn get(&self, doc_id: &str) -> Option<&[f32]> {
        self.entries.iter().find(|e| e.doc_id == doc_id).map(|e| e.v.as_slice())
    }

    /// The source that owns `doc_id`, when known.
    pub fn source_of(&self, doc_id: &str) -> Option<&str> {
        self.entries
            .iter()
            .find(|e| e.doc_id == doc_id)
            .and_then(|e| e.source_id.as_deref())
    }

    /// Sort by `doc_id`, so two backfills of the same index produce byte-identical files.
    pub fn sort(&mut self) {
        self.entries.sort_by(|a, b| a.doc_id.cmp(&b.doc_id));
    }

    /// The `k` documents closest to `query` by cosine similarity, best first.
    ///
    /// Zero vectors (a document whose text was empty, or the zero query) score 0.0 and cannot
    /// rank above a real match. Ties break on `doc_id` so the order is deterministic.
    pub fn top_k(&self, query: &[f32], k: usize) -> Vec<(String, f32)> {
        self.top_k_scoped(query, k, None)
    }

    /// As [`VectorStore::top_k`], optionally restricted to documents owned by `source_id`.
    ///
    /// The restriction is applied *before* truncation, so a scoped call returns the best `k`
    /// within that source rather than the survivors of a global top-k.
    pub fn top_k_scoped(
        &self,
        query: &[f32],
        k: usize,
        source_id: Option<&str>,
    ) -> Vec<(String, f32)> {
        if k == 0 || query.len() != self.dim {
            return Vec::new();
        }
        let qnorm = query.iter().map(|x| x * x).sum::<f32>().sqrt();
        if qnorm == 0.0 {
            return Vec::new();
        }
        let mut scored: Vec<(String, f32)> = self
            .entries
            .iter()
            .filter(|e| match source_id {
                Some(want) => e.source_id.as_deref() == Some(want),
                None => true,
            })
            .map(|e| {
                let dnorm = e.v.iter().map(|x| x * x).sum::<f32>().sqrt();
                let sim = if dnorm == 0.0 {
                    0.0
                } else {
                    let dot: f32 = query.iter().zip(e.v.iter()).map(|(a, b)| a * b).sum();
                    dot / (qnorm * dnorm)
                };
                (e.doc_id.clone(), sim)
            })
            .collect();
        scored.sort_by(|a, b| {
            b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal).then_with(|| a.0.cmp(&b.0))
        });
        scored.truncate(k);
        scored
    }

    /// Write the store. Ids are length-prefixed and vectors are little-endian `f32`.
    pub fn save(&self, path: &Path) -> Result<(), StoreError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut w = BufWriter::new(File::create(path)?);
        w.write_all(MAGIC)?;
        w.write_all(&(self.dim as u32).to_le_bytes())?;
        w.write_all(&(self.entries.len() as u64).to_le_bytes())?;
        for e in &self.entries {
            w.write_all(&(e.doc_id.len() as u32).to_le_bytes())?;
            w.write_all(e.doc_id.as_bytes())?;
            match &e.source_id {
                Some(s) => {
                    w.write_all(&[1u8])?;
                    w.write_all(&(s.len() as u32).to_le_bytes())?;
                    w.write_all(s.as_bytes())?;
                }
                None => w.write_all(&[0u8])?,
            }
            for x in &e.v {
                w.write_all(&x.to_le_bytes())?;
            }
        }
        w.flush()?;
        Ok(())
    }

    /// Read a store written by [`VectorStore::save`].
    /// Load a store if the file exists and parses, else `None`.
    ///
    /// Used as a resume checkpoint: a long embed run replaces the file after every chunk, so an
    /// interrupted run loses at most one chunk. A file that exists but does not parse is treated as
    /// absent — a corrupted checkpoint should restart the run, not wedge it.
    pub fn load_if_present(path: &Path) -> Option<Self> {
        if !path.is_file() {
            return None;
        }
        Self::load(path).ok()
    }

    /// Append another store's entries, for checkpointed incremental builds.
    ///
    /// Dimensions must match; a mismatch drops the incoming store rather than mixing vectors from
    /// two models into one file, which would make every similarity meaningless.
    pub fn extend_from(&mut self, other: &Self) {
        if self.dim != other.dim {
            return;
        }
        for e in &other.entries {
            self.entries.push(e.clone());
        }
    }

    pub fn load(path: &Path) -> Result<Self, StoreError> {
        let mut r = BufReader::new(File::open(path)?);
        let mut magic = [0u8; 8];
        r.read_exact(&mut magic)?;
        if &magic != MAGIC {
            return Err(StoreError::Format("not a vector store (bad magic)".to_string()));
        }
        let dim = read_u32(&mut r)? as usize;
        let count = read_u64(&mut r)? as usize;
        let mut entries = Vec::with_capacity(count);
        for _ in 0..count {
            let id_len = read_u32(&mut r)? as usize;
            let mut id = vec![0u8; id_len];
            r.read_exact(&mut id)?;
            let doc_id = String::from_utf8(id)
                .map_err(|e| StoreError::Format(format!("doc id is not UTF-8: {e}")))?;
            let mut flag = [0u8; 1];
            r.read_exact(&mut flag)?;
            let source_id = if flag[0] == 1 {
                let len = read_u32(&mut r)? as usize;
                let mut s = vec![0u8; len];
                r.read_exact(&mut s)?;
                Some(String::from_utf8(s).map_err(|e| {
                    StoreError::Format(format!("source id is not UTF-8: {e}"))
                })?)
            } else {
                None
            };
            let mut v = Vec::with_capacity(dim);
            for _ in 0..dim {
                let mut b = [0u8; 4];
                r.read_exact(&mut b)?;
                v.push(f32::from_le_bytes(b));
            }
            entries.push(VectorEntry { doc_id, source_id, v });
        }
        Ok(Self { dim, entries })
    }
}

fn read_u32(r: &mut impl Read) -> Result<u32, StoreError> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_le_bytes(b))
}

fn read_u64(r: &mut impl Read) -> Result<u64, StoreError> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b)?;
    Ok(u64::from_le_bytes(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_is_byte_stable_after_sort() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("vectors.bin");

        let mut store = VectorStore::new(3);
        store.insert("d3", None, vec![0.0, 0.0, 1.0]).unwrap();
        store.insert("d1", None, vec![1.0, 0.0, 0.0]).unwrap();
        store.sort();
        store.save(&path).unwrap();

        let loaded = VectorStore::load(&path).unwrap();
        assert_eq!(loaded.dim(), 3);
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded.get("d1"), Some([1.0, 0.0, 0.0].as_slice()));

        // Same content, same order ⇒ the same bytes: a backfill is reproducible.
        let again = tmp.path().join("again.bin");
        loaded.save(&again).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), std::fs::read(&again).unwrap());
    }

    #[test]
    fn top_k_ranks_by_cosine_and_is_deterministic() {
        let mut store = VectorStore::new(2);
        store.insert("near", None, vec![1.0, 0.1]).unwrap();
        store.insert("far", None, vec![0.1, 1.0]).unwrap();
        store.insert("zero", None, vec![0.0, 0.0]).unwrap();

        let hits = store.top_k(&[1.0, 0.0], 3);
        assert_eq!(hits[0].0, "near", "{hits:?}");
        assert_eq!(hits[1].0, "far", "{hits:?}");
        // A zero vector scores 0.0 rather than NaN, so it sorts last and never wins.
        assert_eq!(hits[2], ("zero".to_string(), 0.0));

        // Magnitude is irrelevant (cosine, not dot): the same direction scores the same.
        let scaled = store.top_k(&[10.0, 0.0], 3);
        assert!((scaled[0].1 - hits[0].1).abs() < 1e-6, "{scaled:?} vs {hits:?}");
    }

    #[test]
    fn dimension_mismatch_is_an_error_not_a_silent_truncation() {
        let mut store = VectorStore::new(3);
        let err = store.insert("d1", None, vec![1.0, 2.0]).unwrap_err().to_string();
        assert!(err.contains("dimension mismatch"), "{err}");
        // And a wrong-width query returns nothing rather than mis-ranking.
        assert!(store.top_k(&[1.0, 2.0], 5).is_empty());
    }

    #[test]
    fn a_scoped_query_ranks_within_its_source_only() {
        let mut store = VectorStore::new(2);
        store.insert("a", Some("source-a".into()), vec![1.0, 0.0]).unwrap();
        store.insert("b", Some("source-b".into()), vec![0.9, 0.1]).unwrap();
        store.insert("c", None, vec![1.0, 0.0]).unwrap();

        // Globally, `a` and `c` tie on direction; scoped, only the source's own docs compete.
        let global = store.top_k(&[1.0, 0.0], 3);
        assert_eq!(global.len(), 3);

        let scoped = store.top_k_scoped(&[1.0, 0.0], 3, Some("source-b"));
        assert_eq!(scoped.len(), 1, "only source-b documents may be returned: {scoped:?}");
        assert_eq!(scoped[0].0, "b");

        assert_eq!(store.source_of("a"), Some("source-a"));
        assert_eq!(store.source_of("c"), None);
    }

    #[test]
    fn a_foreign_file_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("not-a-store.bin");
        std::fs::write(&path, b"this is not a vector store at all").unwrap();
        let err = VectorStore::load(&path).unwrap_err().to_string();
        assert!(err.contains("bad magic"), "{err}");
    }
}
