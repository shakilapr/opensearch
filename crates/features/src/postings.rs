//! The inverted feature index: feature id → documents, with quantised activation.
//!
//! v2 §9 asks for "postings keyed by feature id, weights as quantised boosts". This is that
//! structure, built from the encoder's output and searched as a disjunction of the query's
//! features.
//!
//! **Why not Tantivy here, when PLAN-v2 §4 recommends it.** Tantivy can hold the terms, but not
//! the *document-side* weight: a doc's activation would have to be faked into a term frequency
//! (repeat the token) or dropped, and both change the score. The plan anticipated this
//! ("write a custom index only if compression or latency proves inadequate"); what is measured
//! below is the latency, which is the reason it is allowed to be custom.
//!
//! The cost that WAND exists to avoid is proportional to the *sum of the touched posting lists*.
//! Measured on the live fleet: 1.39M documents × 16 active features over 4,096 features gives an
//! average posting list of ~5,400 documents, so a 16-feature query touches ~86K postings — one
//! pass over ~430 KB. That is why there is no block-max skipping yet: the skip structure would
//! cost more to maintain than the scan it avoids. It becomes necessary when the vocabulary grows
//! and the average list length does with it; [`SparseIndex::search`] is the one function to
//! replace then, and the file format already stores each list contiguously so a skip table can be
//! added without a rebuild of the weights.
//!
//! Format (little-endian, byte-stable for the same input):
//!
//! ```text
//! magic "ANSEPOST1" | features u32 | docs u64
//! docs × { id_len u32, id bytes, has_source u8, [source_len u32, source bytes] }
//! features × { count u32, count × { doc u32, q u8 } }
//! ```
//!
//! Documents are written in sorted-id order, so two builds of the same corpus are identical
//! files — the same property the vector sidecar has, for the same reason.

use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::Path;

use rayon::prelude::*;
use thiserror::Error;

use crate::encoder::{Encoder, EncoderError, FeatureHit};

/// Magic header for a postings file.
const MAGIC: &[u8; 9] = b"ANSEPOST1";

/// Errors from building, reading or searching the postings.
#[derive(Debug, Error)]
pub enum PostingsError {
    /// Reading or writing the postings file failed.
    #[error("postings I/O: {0}")]
    Io(#[from] std::io::Error),
    /// The file is not a postings index, or was written by an incompatible version.
    #[error("postings format: {0}")]
    Format(String),
    /// The encoder rejected a document vector.
    #[error(transparent)]
    Encoder(#[from] EncoderError),
}

/// One document's identity in a postings index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocRef {
    /// Document id (the Tantivy delete term, i.e. the resource id).
    pub doc_id: String,
    /// Owning source, when the document has one, so a scoped lane call can honour it exactly.
    pub source_id: Option<String>,
}

/// An inverted index of learned features over one corpus.
#[derive(Debug, Clone)]
pub struct SparseIndex {
    docs: Vec<DocRef>,
    /// `postings[f]` is `(doc index, quantised weight)` sorted by doc index.
    postings: Vec<Vec<(u32, u8)>>,
    /// Document frequency per feature, for the idf weight.
    df: Vec<u32>,
}

impl SparseIndex {
    /// An empty index with room for `features` postings.
    ///
    /// Used by the streaming build, where the document count is only known once every chunk has
    /// been pushed; `build` is a thin wrapper over this so there is one code path for the
    /// postings invariants (ascending doc index, `df` always agreeing with the lists).
    pub fn new(features: usize) -> Self {
        Self { docs: Vec::new(), postings: vec![Vec::new(); features], df: vec![0; features] }
    }

    /// Append one already-coded document and return its index.
    ///
    /// The doc index is the insertion order, so a caller that pushes in sorted-id order gets a
    /// file identical to [`SparseIndex::build`]'s. Hits outside the vocabulary are ignored rather
    /// than panicking: a postings build must not be abortable by a stale encoder.
    pub fn push_coded(
        &mut self,
        doc_id: String,
        source_id: Option<String>,
        hits: &[FeatureHit],
    ) -> u32 {
        let idx = self.docs.len() as u32;
        self.docs.push(DocRef { doc_id, source_id });
        for h in hits {
            let f = h.feature as usize;
            if f >= self.postings.len() {
                continue;
            }
            self.postings[f].push((idx, quantise(h.activation)));
            self.df[f] += 1;
        }
        idx
    }

    /// Build an index over `entries`, keeping the `active` strongest features of each document.
    ///
    /// Entries may arrive in any order; the build sorts by document id so the output file is
    /// reproducible. Documents whose text yielded no vector at all (zero vector) contribute no
    /// postings — an empty document must not become a wildcard match.
    ///
    /// This collects every vector it is given, so it is the right call for a corpus that fits in
    /// memory. `agent0 features build` uses [`SparseIndex::push_coded`] directly instead: a
    /// 350K-document shard's vectors do not fit beside the store they came from, and holding both
    /// made the build die on the memory cgroup rather than finish.
    pub fn build<I>(entries: I, encoder: &Encoder, active: usize) -> Result<Self, PostingsError>
    where
        I: IntoIterator<Item = (String, Option<String>, Vec<f32>)>,
    {
        let mut rows: Vec<(String, Option<String>, Vec<f32>)> = entries.into_iter().collect();
        rows.sort_by(|a, b| a.0.cmp(&b.0));
        rows.dedup_by(|a, b| a.0 == b.0);

        // Encode in parallel: the encoder is immutable here and encoding is the expensive half of
        // a build (one dense pass over W per document).
        let coded: Vec<Vec<FeatureHit>> = rows
            .par_iter()
            .map(|(_, _, v)| encoder.encode(v, active))
            .collect::<Result<Vec<_>, _>>()?;

        let mut index = Self::new(encoder.features());
        for ((doc_id, source_id, _), hits) in rows.into_iter().zip(coded.iter()) {
            index.push_coded(doc_id, source_id, hits);
        }
        Ok(index)
    }

    /// Number of documents.
    pub fn len(&self) -> usize {
        self.docs.len()
    }

    /// Whether the index holds no documents.
    pub fn is_empty(&self) -> bool {
        self.docs.is_empty()
    }

    /// Size of the feature vocabulary.
    pub fn features(&self) -> usize {
        self.postings.len()
    }

    /// Postings in the index, i.e. documents × active features.
    pub fn posting_count(&self) -> usize {
        self.postings.iter().map(Vec::len).sum()
    }

    /// The `n` features with the most postings — the ones whose lists dominate a scan.
    pub fn heaviest_features(&self, n: usize) -> Vec<(u32, u32)> {
        let mut v: Vec<(u32, u32)> = self
            .df
            .iter()
            .enumerate()
            .map(|(f, d)| (f as u32, *d))
            .collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        v.truncate(n);
        v
    }

    /// Rank documents by the query's features.
    ///
    /// `score(d) = Σ_f sqrt(q_f) · (w_df / 255) · idf(f)`, with `idf(f) = ln(1 + N / (1 + df))`.
    /// The square root on the query side stops one strongly-active query feature from dominating a
    /// document that matches several; the idf stops ubiquitous features (which carry little
    /// information about *this* query) from beating distinctive ones.
    ///
    /// Ties break on document id so two runs over the same index return the same order.
    pub fn search(
        &self,
        query: &[FeatureHit],
        limit: usize,
        source_id: Option<&str>,
    ) -> Vec<(String, f32)> {
        if limit == 0 || query.is_empty() || self.docs.is_empty() {
            return Vec::new();
        }
        let n = self.docs.len() as f32;
        let mut acc = vec![0.0f32; self.docs.len()];
        let mut touched: Vec<u32> = Vec::new();
        for h in query {
            let f = h.feature as usize;
            let Some(list) = self.postings.get(f) else {
                continue;
            };
            if list.is_empty() {
                continue;
            }
            let idf = (1.0 + n / (1.0 + self.df[f] as f32)).ln();
            let qw = h.activation.max(0.0).sqrt();
            for (doc, q) in list {
                let slot = &mut acc[*doc as usize];
                if *slot == 0.0 {
                    touched.push(*doc);
                }
                *slot += qw * (*q as f32 / 255.0) * idf;
            }
        }
        let mut out: Vec<(u32, f32)> = touched
            .into_iter()
            .filter(|d| match source_id {
                Some(want) => self.docs[*d as usize].source_id.as_deref() == Some(want),
                None => true,
            })
            .map(|d| (d, acc[d as usize]))
            .filter(|(_, s)| *s > 0.0)
            .collect();
        out.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| {
                    self.docs[a.0 as usize]
                        .doc_id
                        .cmp(&self.docs[b.0 as usize].doc_id)
                })
        });
        out.truncate(limit);
        out.into_iter()
            .map(|(d, s)| (self.docs[d as usize].doc_id.clone(), s))
            .collect()
    }

    /// Documents with the codes stored for them, reconstructed from the postings.
    ///
    /// The postings are the authority: `doc_feature` rows written by `agent0 features build` are
    /// derived from exactly these values, so the forward index and the inverted one cannot
    /// disagree.
    pub fn codes(&self) -> Vec<(String, Vec<FeatureHit>)> {
        let mut per_doc: Vec<Vec<FeatureHit>> = vec![Vec::new(); self.docs.len()];
        for (f, list) in self.postings.iter().enumerate() {
            for (doc, q) in list {
                per_doc[*doc as usize].push(FeatureHit {
                    feature: f as u32,
                    activation: *q as f32 / 255.0,
                });
            }
        }
        self.docs
            .iter()
            .zip(per_doc)
            .map(|(d, hits)| (d.doc_id.clone(), hits))
            .collect()
    }

    /// Write the index. Lists are contiguous and doc ids sorted, so the file is reproducible.
    pub fn save(&self, path: &Path) -> Result<(), PostingsError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut w = BufWriter::new(File::create(path)?);
        w.write_all(MAGIC)?;
        w.write_all(&(self.postings.len() as u32).to_le_bytes())?;
        w.write_all(&(self.docs.len() as u64).to_le_bytes())?;
        for d in &self.docs {
            w.write_all(&(d.doc_id.len() as u32).to_le_bytes())?;
            w.write_all(d.doc_id.as_bytes())?;
            match &d.source_id {
                Some(s) => {
                    w.write_all(&[1u8])?;
                    w.write_all(&(s.len() as u32).to_le_bytes())?;
                    w.write_all(s.as_bytes())?;
                }
                None => w.write_all(&[0u8])?,
            }
        }
        for list in &self.postings {
            w.write_all(&(list.len() as u32).to_le_bytes())?;
            for (doc, q) in list {
                w.write_all(&doc.to_le_bytes())?;
                w.write_all(&[*q])?;
            }
        }
        w.flush()?;
        Ok(())
    }

    /// Read an index written by [`SparseIndex::save`].
    pub fn load(path: &Path) -> Result<Self, PostingsError> {
        let mut r = BufReader::new(File::open(path)?);
        let mut magic = [0u8; 9];
        r.read_exact(&mut magic)?;
        if &magic != MAGIC {
            return Err(PostingsError::Format(
                "not a postings index (bad magic)".into(),
            ));
        }
        let features = read_u32(&mut r)? as usize;
        let docs_len = read_u64(&mut r)? as usize;
        let mut docs = Vec::with_capacity(docs_len);
        for _ in 0..docs_len {
            let id_len = read_u32(&mut r)? as usize;
            let mut id = vec![0u8; id_len];
            r.read_exact(&mut id)?;
            let doc_id = String::from_utf8(id)
                .map_err(|e| PostingsError::Format(format!("doc id is not UTF-8: {e}")))?;
            let mut flag = [0u8; 1];
            r.read_exact(&mut flag)?;
            let source_id =
                if flag[0] == 1 {
                    let len = read_u32(&mut r)? as usize;
                    let mut s = vec![0u8; len];
                    r.read_exact(&mut s)?;
                    Some(String::from_utf8(s).map_err(|e| {
                        PostingsError::Format(format!("source id is not UTF-8: {e}"))
                    })?)
                } else {
                    None
                };
            docs.push(DocRef { doc_id, source_id });
        }
        let mut postings = Vec::with_capacity(features);
        let mut df = vec![0u32; features];
        for f in 0..features {
            let count = read_u32(&mut r)? as usize;
            let mut list = Vec::with_capacity(count);
            for _ in 0..count {
                let doc = read_u32(&mut r)?;
                if doc as usize >= docs_len {
                    return Err(PostingsError::Format(format!(
                        "posting for feature {f} names document {doc} of {docs_len}"
                    )));
                }
                let mut q = [0u8; 1];
                r.read_exact(&mut q)?;
                list.push((doc, q[0]));
            }
            df[f] = count as u32;
            postings.push(list);
        }
        Ok(Self { docs, postings, df })
    }
}

/// Activation in `(0, 1]` → `1..=255`. Zero is reserved for "not in this list", so a feature that
/// fired always occupies a posting — a quantisation that rounded a weak-but-real activation to
/// zero would silently drop the document from retrieval.
fn quantise(activation: f32) -> u8 {
    let q = (activation.clamp(0.0, 1.0) * 255.0).round() as i32;
    q.clamp(1, 255) as u8
}

fn read_u32(r: &mut impl Read) -> Result<u32, PostingsError> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_le_bytes(b))
}

fn read_u64(r: &mut impl Read) -> Result<u64, PostingsError> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b)?;
    Ok(u64::from_le_bytes(b))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoder::{train, TrainOptions};

    fn corpus() -> Vec<(String, Option<String>, Vec<f32>)> {
        (0..200)
            .map(|i| {
                let mut v = vec![0.0f32; 8];
                v[i % 4] = 1.0;
                v[i % 4 + 4] = 0.5;
                // Deterministic jitter: a scrambled index drives the pseudo-random offset without
                // pulling a generator (and its version churn) into a test that must not flake.
                for (j, x) in v.iter_mut().enumerate() {
                    let h = (i * 31 + j * 17) as u64;
                    let u = ((h.wrapping_mul(6364136223846793005) >> 40) as f32
                        / (1u32 << 24) as f32)
                        - 0.5;
                    *x += u * 0.1;
                }
                let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
                let v: Vec<f32> = v.iter().map(|x| x / n).collect();
                let source = if i % 2 == 0 { "s-even" } else { "s-odd" };
                (format!("doc-{i:03}"), Some(source.to_string()), v)
            })
            .collect()
    }

    fn trained() -> Encoder {
        let rows = corpus();
        let samples: Vec<Vec<f32>> = rows.iter().map(|(_, _, v)| v.clone()).collect();
        train(
            &samples,
            8,
            &TrainOptions {
                features: 32,
                active: 4,
                epochs: 8,
                batch: 32,
                ..Default::default()
            },
        )
        .unwrap()
    }

    #[test]
    fn a_query_ranks_the_documents_that_share_its_features() {
        let enc = trained();
        let rows = corpus();
        let index = SparseIndex::build(rows.clone(), &enc, 4).unwrap();
        assert_eq!(index.len(), 200);
        assert_eq!(index.features(), 32);
        assert!(index.posting_count() <= 200 * 4);

        // The corpus has four clusters and the code has four active features, so every document
        // in a cluster carries the *same* code — ordering inside a cluster is a quantisation
        // tie, not a semantic difference. The property that must hold is that the query's
        // cluster fills the top of the ranking, and the query document is in it.
        let probe = 7usize;
        let q = enc.encode(&rows[probe].2, 4).unwrap();
        let hits = index.search(&q, 5, None);
        assert!(!hits.is_empty());
        assert!(hits[0].1 > 0.0);
        let cluster =
            |id: &str| -> usize { id.trim_start_matches("doc-").parse::<usize>().unwrap() % 4 };
        assert!(
            hits.iter()
                .all(|(id, _)| cluster(id) == cluster(&rows[probe].0)),
            "every top hit must come from the query's cluster: {hits:?}"
        );
        // Every document in the cluster carries the same code, so *which* one leads a five-item
        // ranking is a tie-break. Reachability is the real claim: the query document must appear
        // when the whole cluster fits.
        let wide = index.search(&q, 60, None);
        assert!(
            wide.iter().any(|(id, _)| id == &rows[probe].0),
            "the query document must be reachable by its own code"
        );
    }

    #[test]
    fn source_scope_is_applied_before_truncation() {
        let enc = trained();
        let index = SparseIndex::build(corpus(), &enc, 4).unwrap();
        // Query with an odd document's code: features are specific enough that a query for an
        // even document need not reach any odd one at all, which would make the assertion pass
        // vacuously.
        let q = enc.encode(&corpus()[1].2, 4).unwrap();
        let hits = index.search(&q, 50, Some("s-odd"));
        assert!(!hits.is_empty());
        assert!(
            hits.iter().all(|(id, _)| {
                let n: usize = id.trim_start_matches("doc-").parse().unwrap();
                n % 2 == 1
            }),
            "a scoped search must return only that source's documents: {hits:?}"
        );
    }

    #[test]
    fn an_empty_query_returns_nothing() {
        let enc = trained();
        let index = SparseIndex::build(corpus(), &enc, 4).unwrap();
        assert!(index.search(&[], 10, None).is_empty());
        // A zero vector has no positive activation, so it cannot retrieve.
        let q = enc.encode(&vec![0.0; 8], 4).unwrap();
        assert!(q.is_empty());
        assert!(index.search(&q, 10, None).is_empty());
    }

    #[test]
    fn save_and_load_round_trip_byte_stable() {
        let enc = trained();
        let index = SparseIndex::build(corpus(), &enc, 4).unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("postings.bin");
        let b = tmp.path().join("again.bin");
        index.save(&a).unwrap();
        let loaded = SparseIndex::load(&a).unwrap();
        assert_eq!(loaded.len(), index.len());
        assert_eq!(loaded.posting_count(), index.posting_count());
        loaded.save(&b).unwrap();
        assert_eq!(std::fs::read(&a).unwrap(), std::fs::read(&b).unwrap());

        let q = enc.encode(&corpus()[3].2, 4).unwrap();
        assert_eq!(loaded.search(&q, 10, None), index.search(&q, 10, None));
    }

    #[test]
    fn a_foreign_file_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("junk.bin");
        std::fs::write(&p, b"definitely not postings").unwrap();
        let err = SparseIndex::load(&p).unwrap_err().to_string();
        assert!(err.contains("bad magic"), "{err}");
    }

    #[test]
    fn output_is_independent_of_input_order() {
        let enc = trained();
        let mut rows = corpus();
        let a = SparseIndex::build(rows.clone(), &enc, 4).unwrap();
        rows.reverse();
        let b = SparseIndex::build(rows, &enc, 4).unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let (pa, pb) = (tmp.path().join("a.bin"), tmp.path().join("b.bin"));
        a.save(&pa).unwrap();
        b.save(&pb).unwrap();
        assert_eq!(std::fs::read(&pa).unwrap(), std::fs::read(&pb).unwrap());
    }
}
