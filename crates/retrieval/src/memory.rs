//! An in-memory [`LaneSearcher`] with a small BM25 scorer.
//!
//! Hermetic by construction: no Tantivy index, no I/O. Tests and `search-agent0` can drive
//! the source-first lane against this backend. Docs live in a `HashMap` keyed by doc id; a
//! search tokenizes the query and each document, then scores with a standard BM25
//! (k1 = 1.2, b = 0.75) over the in-memory corpus. Ties break by doc id so output is
//! deterministic.

use std::collections::HashMap;

use crate::{Candidate, Lane, LaneSearcher, RetrievalError};

/// One document held by [`MemoryLaneSearcher`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryDoc {
    pub doc_id: String,
    pub title: String,
    pub body: String,
    pub source_id: Option<String>,
    pub url: Option<String>,
}

impl MemoryDoc {
    /// A document with just an id and title (empty body, no source).
    pub fn new(doc_id: impl Into<String>, title: impl Into<String>) -> Self {
        Self {
            doc_id: doc_id.into(),
            title: title.into(),
            body: String::new(),
            source_id: None,
            url: None,
        }
    }

    pub fn with_body(mut self, body: impl Into<String>) -> Self {
        self.body = body.into();
        self
    }

    pub fn with_source(mut self, source_id: impl Into<String>) -> Self {
        self.source_id = Some(source_id.into());
        self
    }

    pub fn with_url(mut self, url: impl Into<String>) -> Self {
        self.url = Some(url.into());
        self
    }

    /// The BM25 field: title repeated once (a mild title boost) plus the body.
    fn field_text(&self) -> String {
        if self.body.is_empty() {
            self.title.clone()
        } else {
            format!("{} {}", self.title, self.body)
        }
    }
}

/// BM25 term-frequency saturation / length-normalization constants.
const K1: f32 = 1.2;
const B: f32 = 0.75;

/// A hermetic, Tantivy-free [`LaneSearcher`] over an in-memory document set.
#[derive(Debug, Clone, Default)]
pub struct MemoryLaneSearcher {
    docs: HashMap<String, MemoryDoc>,
    /// Insertion order, so tie-breaks and iteration are stable across runs.
    order: Vec<String>,
    /// Lane tag stamped on emitted candidates (default [`Lane::Bm25`]).
    lane: Lane,
}

impl MemoryLaneSearcher {
    pub fn new() -> Self {
        Self {
            docs: HashMap::new(),
            order: Vec::new(),
            lane: Lane::Bm25,
        }
    }

    /// Build from an iterator of documents (later duplicates of a doc id replace earlier).
    pub fn with_docs(docs: impl IntoIterator<Item = MemoryDoc>) -> Self {
        let mut s = Self::new();
        for d in docs {
            s.add_doc(d);
        }
        s
    }

    /// Insert or replace a document by id.
    pub fn add_doc(&mut self, doc: MemoryDoc) {
        if !self.docs.contains_key(&doc.doc_id) {
            self.order.push(doc.doc_id.clone());
        }
        self.docs.insert(doc.doc_id.clone(), doc);
    }

    /// Builder: tag emitted candidates with a specific lane.
    pub fn with_lane(mut self, lane: Lane) -> Self {
        self.lane = lane;
        self
    }

    pub fn len(&self) -> usize {
        self.docs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.docs.is_empty()
    }

    /// The documents in insertion order (deterministic).
    pub fn docs(&self) -> impl Iterator<Item = &MemoryDoc> {
        self.order.iter().filter_map(move |id| self.docs.get(id))
    }
}

/// Lowercase and split on any non-alphanumeric byte. ASCII-only, which is enough for the
/// hermetic corpus this backend serves.
fn tokenize(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(|t| t.to_ascii_lowercase())
        .collect()
}

impl LaneSearcher for MemoryLaneSearcher {
    fn search(
        &self,
        query: &str,
        limit: usize,
        source_id: Option<&str>,
    ) -> Result<Vec<Candidate>, RetrievalError> {
        if limit == 0 {
            return Ok(Vec::new());
        }

        // Documents visible to this call (source scope applied first).
        let scoped: Vec<&MemoryDoc> = self
            .docs()
            .filter(|d| match source_id {
                Some(s) => d.source_id.as_deref() == Some(s),
                None => true,
            })
            .collect();
        if scoped.is_empty() {
            return Ok(Vec::new());
        }

        // Pre-tokenize and gather corpus statistics.
        struct DocStats<'a> {
            doc: &'a MemoryDoc,
            len: usize,
            tf: HashMap<String, u32>,
        }
        let mut stats: Vec<DocStats> = Vec::with_capacity(scoped.len());
        let mut df: HashMap<String, u32> = HashMap::new();
        let mut total_len = 0usize;
        for doc in scoped {
            let toks = tokenize(&doc.field_text());
            let mut tf: HashMap<String, u32> = HashMap::new();
            for t in &toks {
                *tf.entry(t.clone()).or_insert(0) += 1;
            }
            for t in tf.keys() {
                *df.entry(t.clone()).or_insert(0) += 1;
            }
            total_len += toks.len();
            stats.push(DocStats {
                doc,
                len: toks.len(),
                tf,
            });
        }
        let n = stats.len() as f32;
        let avgdl = (total_len as f32 / n).max(f32::EPSILON);

        let query_terms = tokenize(query);
        let mut scored: Vec<(f32, &MemoryDoc)> = Vec::new();
        for ds in &stats {
            // Documents with no query term at all are not hits.
            if query_terms.iter().all(|qt| ds.tf.get(qt).copied().unwrap_or(0) == 0) {
                continue;
            }
            let dl = ds.len as f32;
            let mut score = 0.0f32;
            for qt in &query_terms {
                let tfv = ds.tf.get(qt).copied().unwrap_or(0);
                if tfv == 0 {
                    continue;
                }
                let dfv = df.get(qt).copied().unwrap_or(0) as f32;
                let idf = (1.0 + (n - dfv + 0.5) / (dfv + 0.5)).ln();
                let tf = tfv as f32;
                let denom = tf + K1 * (1.0 - B + B * dl / avgdl);
                score += idf * (tf * (K1 + 1.0)) / denom;
            }
            scored.push((score, ds.doc));
        }

        // Score desc, then doc id asc — fully deterministic.
        scored.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.doc_id.cmp(&b.1.doc_id)));

        Ok(scored
            .into_iter()
            .take(limit)
            .map(|(score, doc)| Candidate {
                doc_id: doc.doc_id.clone(),
                score,
                lane: self.lane,
                source_id: doc.source_id.clone(),
                title: Some(doc.title.clone()),
                url: doc.url.clone(),
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenize_splits_and_lowercases() {
        assert_eq!(tokenize("Rust, Async! LLM_2024"), vec!["rust", "async", "llm", "2024"]);
        assert!(tokenize("   ").is_empty());
    }

    #[test]
    fn empty_corpus_returns_no_hits() {
        let s = MemoryLaneSearcher::new();
        assert!(s.search("anything", 10, None).unwrap().is_empty());
        assert_eq!(s.len(), 0);
        assert!(s.is_empty());
    }

    #[test]
    fn docs_iterates_in_insertion_order() {
        let s = MemoryLaneSearcher::with_docs(vec![
            MemoryDoc::new("z", "Z"),
            MemoryDoc::new("a", "A"),
        ]);
        let ids: Vec<&str> = s.docs().map(|d| d.doc_id.as_str()).collect();
        assert_eq!(ids, vec!["z", "a"]);
    }
}
