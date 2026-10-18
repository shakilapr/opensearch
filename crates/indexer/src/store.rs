//! Tantivy base + overlay store (spec §6).
//!
//! Two indices are searched together:
//!   * `base`    — immutable-ish, written in batches.
//!   * `overlay` — recent docs, written one at a time for freshness.
//!
//! `doc_id` is the delete term. On update, the base `doc_id` term is deleted and
//! the new version is written to the overlay, where it shadows the base copy by
//! `version`. Fold periodically moves overlay docs into base and clears overlay.
//!
//! Commit/reload policy: writers `commit()`, and readers `reload()` before every
//! search so a fresh commit is visible without reopening the index.

use crate::doc::IndexDoc;
use crate::schema::build_schema;
use search_core::contract::Signal;
use search_core::{AppError, AppResult, IndexManifest, Provenance, SearchResult};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use tantivy::collector::{DocSetCollector, TopDocs};
use tantivy::snippet::SnippetGenerator;
use tantivy::query::{AllQuery, QueryParser, TermQuery};
use tantivy::schema::{Field, Schema, TantivyDocument, Value};
use tantivy::{DocAddress, Index, IndexReader, IndexWriter, Searcher, Term};

/// Characters kept from a query-relevant fragment (the UI and the answerer both
/// read this). First-N-of-body was tried first and reads as boilerplate.
const SNIPPET_CHARS: usize = 300;

/// Writer arena budget per index. Tantivy needs >= a few MiB per thread.
const WRITER_BUDGET: usize = 50_000_000;
const MANIFEST_FILE: &str = "_manifest.json";
const SCHEMA_HASH_FILE: &str = "_schema_hash";

/// Paths for the two indices.
#[derive(Debug, Clone)]
pub struct IndexerConfig {
    pub base_dir: PathBuf,
    pub overlay_dir: PathBuf,
    /// Open without an `IndexWriter`, so the process takes **no lock** on the directory.
    ///
    /// Tantivy's writer lock is exclusive per index directory, which is why a crawl and a
    /// `serve` could not run against the same data root at all. A read-only index serves
    /// queries normally and refuses every write with a clear error.
    pub read_only: bool,
}

impl IndexerConfig {
    /// The spec's default layout under a `tantivy/` root.
    pub fn new(root: impl AsRef<Path>) -> Self {
        let root = root.as_ref();
        Self {
            base_dir: root.join("base"),
            overlay_dir: root.join("overlay"),
            read_only: false,
        }
    }

    /// The same layout, opened read-only (no writer, no lock).
    pub fn read_only(root: impl AsRef<Path>) -> Self {
        Self { read_only: true, ..Self::new(root) }
    }
}

/// Field handles resolved once from the frozen schema.
///
/// A complete 1:1 mirror of the schema: some handles are only needed for
/// documentation/future fields, so silence dead-code for the ones not read yet.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy)]
struct Fields {
    doc_id: Field,
    version: Field,
    resource_id: Field,
    work_id: Field,
    content_id: Field,
    url: Field,
    title: Field,
    title_boost: Field,
    headings: Field,
    body: Field,
    body_boost: Field,
    entities: Field,
    source_id: Field,
    doc_type: Field,
    language: Field,
    published_at: Field,
    modified_at: Field,
    fetched_at: Field,
    visible_generation: Field,
    tombstone: Field,
}

impl Fields {
    fn from_schema(schema: &Schema) -> Self {
        let g = |n: &str| schema.get_field(n).expect("frozen schema field missing");
        Self {
            doc_id: g("doc_id"),
            version: g("version"),
            resource_id: g("resource_id"),
            work_id: g("work_id"),
            content_id: g("content_id"),
            url: g("url"),
            title: g("title"),
            title_boost: g("title_boost"),
            headings: g("headings"),
            body: g("body"),
            body_boost: g("body_boost"),
            entities: g("entities"),
            source_id: g("source_id"),
            doc_type: g("doc_type"),
            language: g("language"),
            published_at: g("published_at"),
            modified_at: g("modified_at"),
            fetched_at: g("fetched_at"),
            visible_generation: g("visible_generation"),
            tombstone: g("tombstone"),
        }
    }
}

#[derive(Default)]
struct State {
    /// Last published manifest; its `generation` is the search watermark.
    manifest: Option<IndexManifest>,
}

/// A scored candidate assembled from one index (before cross-index merge).
struct Candidate {
    doc_id: String,
    version: u64,
    resource_id: String,
    work_id: Option<String>,
    content_id: Option<String>,
    url: String,
    title: String,
    body: String,
    /// Query-relevant excerpt of `body`, set by `collect`; empty means "not
    /// generated" and falls back to the leading characters of the body.
    snippet: String,
    source_id: Option<String>,
    published_at: Option<i64>,
    modified_at: Option<i64>,
    fetched_at: i64,
    visible_generation: u64,
    tombstone: u64,
    score: f32,
    provenance: Provenance,
}

impl Candidate {
    fn from_doc(
        searcher: &Searcher,
        addr: DocAddress,
        f: &Fields,
        score: f32,
        provenance: Provenance,
    ) -> AppResult<Self> {
        let doc: TantivyDocument = searcher.doc(addr).ix()?;
        let text = |field: Field| -> String {
            doc.get_first(field)
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string()
        };
        let opt = |field: Field| -> Option<String> {
            let s = text(field);
            if s.is_empty() {
                None
            } else {
                Some(s)
            }
        };
        let i64_opt = |field: Field| -> Option<i64> {
            doc.get_first(field)
                .and_then(|v| v.as_i64())
                .filter(|v| *v >= 0)
        };
        // `version`, `visible_generation` and `tombstone` are FAST-only (not
        // STORED), so they must be read from the columnar fast fields.
        Ok(Self {
            doc_id: text(f.doc_id),
            version: fast_u64(searcher, addr, "version"),
            resource_id: text(f.resource_id),
            work_id: opt(f.work_id),
            content_id: opt(f.content_id),
            url: text(f.url),
            title: text(f.title),
            body: text(f.body),
            snippet: String::new(),
            source_id: opt(f.source_id),
            published_at: i64_opt(f.published_at),
            modified_at: i64_opt(f.modified_at),
            fetched_at: doc
                .get_first(f.fetched_at)
                .and_then(|v| v.as_i64())
                .unwrap_or(0),
            visible_generation: fast_u64(searcher, addr, "visible_generation"),
            tombstone: fast_u64(searcher, addr, "tombstone"),
            score,
            provenance,
        })
    }

    /// Whether `self` should replace `other` for the same `doc_id`:
    /// highest version wins; ties break to the overlay (fresher).
    fn beats(&self, other: &Candidate) -> bool {
        if self.version != other.version {
            return self.version > other.version;
        }
        let self_overlay = self.provenance == Provenance::Overlay;
        let other_overlay = other.provenance == Provenance::Overlay;
        self_overlay && !other_overlay
    }

    fn into_result(self) -> SearchResult {
        let lane = match self.provenance {
            Provenance::Overlay => "lexical_overlay",
            Provenance::Snapshot => "lexical_base",
        };
        let snippet = if self.snippet.trim().is_empty() {
            make_snippet(&self.body)
        } else {
            self.snippet
        };
        SearchResult {
            doc_id: self.doc_id,
            version: self.version,
            resource_id: self.resource_id,
            work_id: self.work_id,
            content_id: self.content_id,
            canonical_url: self.url,
            title: self.title,
            snippet,
            source_id: self.source_id,
            published_at: self.published_at,
            modified_at: self.modified_at,
            fetched_at: self.fetched_at,
            lane: lane.to_string(),
            provenance: self.provenance,
            score: self.score,
            signals: vec![Signal {
                name: "bm25".to_string(),
                value: self.score,
                note: "lexical relevance (title_boost/body_boost)".to_string(),
            }],
        }
    }
}

/// The indexer owns an open base + overlay and their writers/readers.
pub struct Indexer {
    schema: Schema,
    fields: Fields,
    base_dir: PathBuf,
    overlay_dir: PathBuf,
    base: Index,
    overlay: Index,
    /// `None` when the index was opened read-only (see `IndexerConfig::read_only`): no writer
    /// means no lock on the directory, which is what lets a reader run beside a crawl.
    base_writer: Option<Mutex<IndexWriter>>,
    overlay_writer: Option<Mutex<IndexWriter>>,
    base_reader: IndexReader,
    overlay_reader: IndexReader,
    state: Mutex<State>,
}

impl Indexer {
    /// Open (creating if absent) both indices, asserting the frozen schema.
    pub fn open(cfg: IndexerConfig) -> AppResult<Self> {
        let schema = build_schema();
        let base = open_or_create(&cfg.base_dir, &schema)?;
        let overlay = open_or_create(&cfg.overlay_dir, &schema)?;
        // The writer is what takes the exclusive lock, so a read-only open must not create one.
        let (base_writer, overlay_writer) = if cfg.read_only {
            (None, None)
        } else {
            (
                Some(Mutex::new(base.writer(WRITER_BUDGET).ix()?)),
                Some(Mutex::new(overlay.writer(WRITER_BUDGET).ix()?)),
            )
        };
        let base_reader = base.reader().ix()?;
        let overlay_reader = overlay.reader().ix()?;
        let fields = Fields::from_schema(&schema);
        let manifest = load_manifest(&cfg.base_dir)?;
        Ok(Self {
            schema,
            fields,
            base_dir: cfg.base_dir,
            overlay_dir: cfg.overlay_dir,
            base,
            overlay,
            base_writer,
            overlay_writer,
            base_reader,
            overlay_reader,
            state: Mutex::new(State { manifest }),
        })
    }

    pub fn schema(&self) -> &Schema {
        &self.schema
    }
    pub fn base_dir(&self) -> &Path {
        &self.base_dir
    }
    pub fn overlay_dir(&self) -> &Path {
        &self.overlay_dir
    }

    /// Whether this handle can write (false = opened read-only, no writer, no lock).
    pub fn is_read_only(&self) -> bool {
        self.base_writer.is_none()
    }

    /// The base writer, or a clear error when the index is open read-only.
    ///
    /// A read-only index exists so a reader can run beside a crawler; every write path must
    /// therefore fail with a sentence a caller can log, not a panic or a silent no-op.
    fn base_writer(&self) -> AppResult<std::sync::MutexGuard<'_, IndexWriter>> {
        match self.base_writer.as_ref() {
            Some(w) => Ok(w.lock().unwrap_or_else(|e| e.into_inner())),
            None => Err(AppError::Index(
                "index is open read-only: writes are refused (start without read-only to write)"
                    .to_string(),
            )),
        }
    }

    /// The overlay writer, or a clear error when the index is open read-only.
    fn overlay_writer(&self) -> AppResult<std::sync::MutexGuard<'_, IndexWriter>> {
        match self.overlay_writer.as_ref() {
            Some(w) => Ok(w.lock().unwrap_or_else(|e| e.into_inner())),
            None => Err(AppError::Index(
                "index is open read-only: writes are refused (start without read-only to write)"
                    .to_string(),
            )),
        }
    }

    // -- writes ------------------------------------------------------------

    /// Single-doc writer on the overlay, for freshness. Commits immediately.
    pub fn index_overlay(&self, doc: IndexDoc) -> AppResult<()> {
        let mut w = self.overlay_writer()?;
        // Replace any prior overlay version of this doc (doc_id is the delete term).
        w.delete_term(Term::from_field_text(self.fields.doc_id, &doc.doc_id));
        w.add_document(doc.to_tantivy(&self.schema)).ix()?;
        w.commit().ix()?;
        Ok(())
    }

    /// Batched writer on the overlay: add every doc, then commit **once**.
    ///
    /// Same delete-then-add semantics as [`Indexer::index_overlay`], but a crawl
    /// that indexes thousands of pages must not pay a fresh Tantivy segment (and
    /// the O(index) manifest scan in [`Indexer::publish_manifest`]) per page —
    /// measured at ~2 s per document, which capped a 4-shard fleet at ~110
    /// docs/min while every core sat idle. One commit per batch removes that
    /// ceiling without changing what is indexed or when it becomes visible
    /// (visibility still comes from the manifest the caller publishes).
    pub fn index_overlay_batch(&self, docs: Vec<IndexDoc>) -> AppResult<()> {
        if docs.is_empty() {
            return Ok(());
        }
        let mut w = self.overlay_writer()?;
        for doc in docs {
            w.delete_term(Term::from_field_text(self.fields.doc_id, &doc.doc_id));
            w.add_document(doc.to_tantivy(&self.schema)).ix()?;
        }
        w.commit().ix()?;
        Ok(())
    }

    /// Batched writer on the base index. One commit for the whole batch.
    pub fn index_batch(&self, docs: Vec<IndexDoc>) -> AppResult<()> {
        let mut w = self.base_writer()?;
        for doc in docs {
            w.delete_term(Term::from_field_text(self.fields.doc_id, &doc.doc_id));
            w.add_document(doc.to_tantivy(&self.schema)).ix()?;
        }
        w.commit().ix()?;
        Ok(())
    }

    /// Delete a doc: remove it from base and write a shadowing tombstone into
    /// the overlay (spec §6 delete semantics — history is not erased).
    pub fn delete(&self, doc_id: &str) -> AppResult<()> {
        let version = self.current_version(doc_id)?.saturating_add(1);
        {
            let mut w = self.base_writer()?;
            w.delete_term(Term::from_field_text(self.fields.doc_id, doc_id));
            w.commit().ix()?;
        }
        self.index_overlay(IndexDoc::tombstone(doc_id, version))
    }

    /// Delete the `doc_id` term from base only.
    pub fn delete_from_base(&self, doc_id: &str) -> AppResult<()> {
        let mut w = self.base_writer()?;
        w.delete_term(Term::from_field_text(self.fields.doc_id, doc_id));
        w.commit().ix()?;
        Ok(())
    }

    /// Highest version currently visible for `doc_id` across both indices.
    fn current_version(&self, doc_id: &str) -> AppResult<u64> {
        self.base_reader.reload().ix()?;
        self.overlay_reader.reload().ix()?;
        let term = Term::from_field_text(self.fields.doc_id, doc_id);
        let mut best = 0u64;
        for reader in [&self.base_reader, &self.overlay_reader] {
            let searcher = reader.searcher();
            let query = tantivy::query::TermQuery::new(
                term.clone(),
                tantivy::schema::IndexRecordOption::Basic,
            );
            let addrs = searcher
                .search(&query, &DocSetCollector)
                .ix()?;
            for addr in addrs {
                best = best.max(fast_u64(&searcher, addr, "version"));
            }
        }
        Ok(best)
    }

    // -- reads -------------------------------------------------------------

    /// Published generation watermark; docs above it are not exposed yet.
    pub fn visible_generation(&self) -> u64 {
        self.state
            .lock()
            .unwrap()
            .manifest
            .as_ref()
            .map(|m| m.generation)
            .unwrap_or(0)
    }

    /// Current overlay watermark (same as the visible generation in Stage 0).
    pub fn overlay_watermark(&self) -> u64 {
        self.visible_generation()
    }

    /// The last published manifest, read back from the persisted
    /// `_manifest.json` the indexer writes atomically on every publish.
    ///
    /// Returns `None` until the first publish — an unpublished index has no
    /// manifest and callers must not fabricate one. This is the read-side
    /// counterpart to [`Indexer::publish_manifest`], so other crates never
    /// need to know the on-disk manifest location.
    pub fn manifest(&self) -> AppResult<Option<IndexManifest>> {
        load_manifest(&self.base_dir)
    }

    /// Search base + overlay, dedupe by `doc_id` keeping the newest version,
    /// drop tombstones and not-yet-visible docs, tag each hit with provenance.
    pub fn search(&self, query: &str, limit: usize) -> AppResult<Vec<SearchResult>> {
        self.base_reader.reload().ix()?;
        self.overlay_reader.reload().ix()?;
        let watermark = self.visible_generation();

        let fetch = limit.clamp(1, 1000) * 4 + 16;
        let mut by_id: HashMap<String, Candidate> = HashMap::new();
        self.collect(
            &self.base,
            &self.base_reader,
            query,
            fetch,
            Provenance::Snapshot,
            &mut by_id,
        )?;
        self.collect(
            &self.overlay,
            &self.overlay_reader,
            query,
            fetch,
            Provenance::Overlay,
            &mut by_id,
        )?;

        let mut hits: Vec<Candidate> = by_id
            .into_values()
            .filter(|c| c.tombstone == 0 && c.visible_generation <= watermark)
            .collect();
        hits.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| b.version.cmp(&a.version))
                .then_with(|| a.doc_id.cmp(&b.doc_id))
        });
        hits.truncate(limit);
        Ok(hits.into_iter().map(Candidate::into_result).collect())
    }

    fn collect(
        &self,
        index: &Index,
        reader: &IndexReader,
        query: &str,
        fetch: usize,
        provenance: Provenance,
        out: &mut HashMap<String, Candidate>,
    ) -> AppResult<()> {
        let searcher = reader.searcher();
        // BM25 with the boosted duplicate fields (spec §6).
        // Every field the index actually holds is searched, including the *structured* ones.
        // `headings` (which carries the organizing model's summary and topics) and `entities` (its
        // key entities) were in the schema but absent from this list, so the whole point of paying a
        // model to structure a page — that its entities and topics become findable — had no effect
        // on retrieval. The boosts say what the engine believes: a page whose entities or topics
        // name the query is more on-topic than one that merely mentions the word in prose.
        let mut parser = QueryParser::for_index(
            index,
            vec![
                self.fields.title_boost,
                self.fields.headings,
                self.fields.entities,
                self.fields.body_boost,
                self.fields.url,
            ],
        );
        parser.set_field_boost(self.fields.title_boost, 3.0);
        parser.set_field_boost(self.fields.entities, 3.0);
        parser.set_field_boost(self.fields.headings, 2.0);
        parser.set_field_boost(self.fields.body_boost, 1.0);
        // Parse the pruned query: the index tokenizer keeps stop-words and
        // single letters, over which an OR query degenerates into noise.
        let pruned = search_core::text::prune_query(query);
        let parsed = match parser.parse_query(&pruned) {
            Ok(q) => q,
            // A query we can't parse contributes no candidates rather than failing
            // the whole search (base search must still work).
            Err(_) => return Ok(()),
        };
        let top = searcher.search(&parsed, &TopDocs::with_limit(fetch)).ix()?;
        // Query-relevant fragments: the first characters of a page are usually
        // navigation/boilerplate, which reads as "just headers" to a downstream
        // answerer. When Tantivy can build a snippet generator for the query, the
        // stored body is excerpted around the matched terms instead.
        // The fragment must be built from a field that is BOTH indexed with
        // positions and STORED, because `snippet_from_doc` re-reads the value
        // from the retrieved document: `body_boost` (the field the parser
        // searches) is not STORED, so the generator is built from a `body`-only
        // parse of the same query. No body parse -> no fragment -> head fallback.
        let body_parser = QueryParser::for_index(index, vec![self.fields.body]);
        let body_query = body_parser.parse_query(&pruned).ok();
        let mut generator = match body_query {
            Some(q) => SnippetGenerator::create(&searcher, &q, self.fields.body)
                .ok()
                .map(|mut g| {
                    g.set_max_num_chars(SNIPPET_CHARS);
                    g
                }),
            None => None,
        };
        for (score, addr) in top {
            let mut cand = Candidate::from_doc(&searcher, addr, &self.fields, score, provenance)?;
            if let Some(g) = generator.as_mut() {
                let stored: TantivyDocument = searcher.doc(addr).ix()?;
                // `.fragment()`, not `.to_html()`: the HTML form wraps every matched term in
                // `<b>`/`</b>`, which is markup the snippet contract does not promise and a
                // text-rendering client shows literally — a live result read
                // "… DAST <b>winter</b> update 2025 …". Highlighting is a presentation decision, and
                // this one is not made here: the snippet is plain text.
                let fragment = g.snippet_from_doc(&stored).fragment().to_string();
                if !fragment.trim().is_empty() {
                    cand.snippet = fragment;
                }
            }
            match out.get(&cand.doc_id) {
                Some(existing) if !cand.beats(existing) => {}
                _ => {
                    out.insert(cand.doc_id.clone(), cand);
                }
            }
        }
        Ok(())
    }

    // -- fold / manifest ---------------------------------------------------

    /// Merge overlay docs into base, clear the overlay, bump the generation and
    /// publish a manifest row. Returned manifest reflects the new generation.
    pub fn fold_overlay_into_base(&self) -> AppResult<IndexManifest> {
        let new_gen = self.visible_generation() + 1;

        // Read every overlay doc (including tombstones) while holding the
        // overlay writer lock is avoided; readers are independent.
        self.overlay_reader.reload().ix()?;
        let overlay_docs = self.read_all(&self.overlay_reader, Provenance::Overlay)?;

        {
            let mut w = self.base_writer()?;
            for cand in &overlay_docs {
                w.delete_term(Term::from_field_text(self.fields.doc_id, &cand.doc_id));
                if cand.tombstone == 0 {
                    // Re-materialize as an IndexDoc at the new generation.
                    let doc = cand.to_index_doc(new_gen);
                    w.add_document(doc.to_tantivy(&self.schema)).ix()?;
                }
                // Tombstones are dropped from base (the term is already removed).
            }
            w.commit().ix()?;
        }

        // Clear the overlay in one commit.
        {
            let mut w = self.overlay_writer()?;
            w.delete_all_documents().ix()?;
            w.commit().ix()?;
        }
        self.base_reader.reload().ix()?;
        self.overlay_reader.reload().ix()?;

        self.publish_manifest()
    }

    /// Every live (non-deleted) doc currently held by a reader.
    fn read_all(&self, reader: &IndexReader, provenance: Provenance) -> AppResult<Vec<Candidate>> {
        let searcher = reader.searcher();
        let addrs = searcher.search(&AllQuery, &DocSetCollector).ix()?;
        let mut out = Vec::with_capacity(addrs.len());
        for addr in addrs {
            out.push(Candidate::from_doc(&searcher, addr, &self.fields, 0.0, provenance)?);
        }
        Ok(out)
    }

    /// The stored body text of one document, if this index holds it.
    ///
    /// The overlay is consulted first (a newer version wins), then the base. A
    /// tombstoned row is reported as absent. Used by the answer path, which must
    /// read real content rather than a leading-characters fragment; the caller
    /// bounds how much of the returned text it uses.
    pub fn doc_text(&self, doc_id: &str) -> AppResult<Option<String>> {
        for reader in [&self.overlay_reader, &self.base_reader] {
            reader.reload().ix()?;
            let searcher = reader.searcher();
            let term = Term::from_field_text(self.fields.doc_id, doc_id);
            let query =
                TermQuery::new(term, tantivy::schema::IndexRecordOption::Basic);
            let top = searcher.search(&query, &TopDocs::with_limit(1)).ix()?;
            let Some((_, addr)) = top.first() else {
                continue;
            };
            if fast_u64(&searcher, *addr, "tombstone") != 0 {
                return Ok(None);
            }
            let doc: TantivyDocument = searcher.doc(*addr).ix()?;
            let body = doc
                .get_first(self.fields.body)
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            return Ok(Some(body.to_string()));
        }
        Ok(None)
    }

    /// Every live (non-tombstoned, visible) document in base ∪ overlay as full
    /// `IndexDoc`s — everything the schema STOREs (`url`, `title`, `body`,
    /// `source_id`, `published_at`, ids), i.e. enough to rebuild a document
    /// without touching the blob store. Deduped by `doc_id`: a newer overlay copy
    /// wins over the base copy, exactly as `search` does it.
    ///
    /// Order is `doc_id`-sorted so two runs over the same index are byte-identical.
    ///
    /// Lossy by design: `headings` and `entities` are not STOREd, so they come back
    /// empty (see [`Candidate::to_index_doc`]).
    pub fn all_docs(&self) -> AppResult<Vec<IndexDoc>> {
        self.base_reader.reload().ix()?;
        self.overlay_reader.reload().ix()?;
        let watermark = self.visible_generation();

        let mut by_id: HashMap<String, Candidate> = HashMap::new();
        for cand in self.read_all(&self.base_reader, Provenance::Snapshot)? {
            by_id.insert(cand.doc_id.clone(), cand);
        }
        for cand in self.read_all(&self.overlay_reader, Provenance::Overlay)? {
            match by_id.get(&cand.doc_id) {
                Some(existing) if !cand.beats(existing) => {}
                _ => {
                    by_id.insert(cand.doc_id.clone(), cand);
                }
            }
        }

        let mut docs: Vec<IndexDoc> = by_id
            .into_values()
            .filter(|c| c.tombstone == 0 && c.visible_generation <= watermark)
            .map(|c| c.to_index_doc(c.visible_generation))
            .collect();
        docs.sort_by(|a, b| a.doc_id.cmp(&b.doc_id));
        Ok(docs)
    }

    /// Doc count (base ∪ overlay, deduped by `doc_id`, tombstones excluded) and base segments.
    ///
    /// This is the number `publish_manifest` reports, and it must not cost O(corpus). The previous
    /// implementation ran `AllQuery` over both indices and materialised every stored field, so each
    /// publish grew with the index — and because ingest published once per batch (and once per
    /// document before that), total work was quadratic.
    ///
    /// `Searcher::num_docs()` returns the number of *alive* documents (tantivy excludes deletes),
    /// so the base half is O(segments). Only the overlay needs a predicate: it is where documents
    /// that are not yet published live, and an overlay copy shadows its base copy, so counting both
    /// would double-count. `published_generation` is the generation this publish is creating — a
    /// document stamped with it becomes visible in the same step, so it counts.
    ///
    /// Cost: O(segments) + O(overlay).
    fn stats(&self, published_generation: u64) -> AppResult<(u64, u64)> {
        let base_searcher = self.base_reader.searcher();
        let overlay_searcher = self.overlay_reader.searcher();
        let base_segments = base_searcher.segment_readers().len() as u64;

        // Every base document is published by construction (a write there is followed by a publish).
        let mut live = base_searcher.num_docs();

        for addr in overlay_searcher.search(&AllQuery, &DocSetCollector).ix()? {
            if fast_u64(&overlay_searcher, addr, "tombstone") != 0 {
                continue;
            }
            if fast_u64(&overlay_searcher, addr, "visible_generation") > published_generation {
                continue;
            }
            let doc: TantivyDocument = overlay_searcher.doc(addr).ix()?;
            let id = doc
                .get_first(self.fields.doc_id)
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            if id.is_empty() {
                continue;
            }
            // One live document that exists in both indices: the overlay copy replaces the base
            // copy, which `num_docs()` above already counted.
            let term = Term::from_field_text(self.fields.doc_id, id);
            let shadows_base = !base_searcher
                .search(
                    &TermQuery::new(term, tantivy::schema::IndexRecordOption::Basic),
                    &DocSetCollector,
                )
                .ix()?
                .is_empty();
            if !shadows_base {
                live += 1;
            }
        }
        Ok((live, base_segments))
    }

    /// Publish a manifest row: generation = prev + 1, with doc/segment counts.
    /// Persisted so it survives a reopen (persistence).
    pub fn publish_manifest_at(&self, generation: u64) -> AppResult<IndexManifest> {
        self.base_reader.reload().ix()?;
        self.overlay_reader.reload().ix()?;

        let mut state = self.state.lock().unwrap();
        // Count for the generation being published, before the manifest exposes it.
        let (doc_count, base_segments) = self.stats(generation)?;
        let manifest = IndexManifest {
            manifest_id: format!("gen-{generation}"),
            generation,
            doc_count,
            base_segments,
            watermark: generation,
            embedding_model_ids: Vec::new(),
            metadata_version: generation,
            created_at: search_core::time::now_unix_s(),
            published_at: Some(search_core::time::now_unix_s()),
        };
        write_manifest(&self.base_dir, &manifest)?;
        state.manifest = Some(manifest.clone());
        tracing::info!(generation, doc_count, base_segments, "published index manifest");
        Ok(manifest)
    }

    /// Publish the next generation, for callers with no external allocator (tests, one-shot use).
    ///
    /// Ingest must not use this: deriving the generation from the in-memory manifest lets a
    /// generation be reused after a crash between the commit and the publish, which is exactly how
    /// an abandoned batch becomes visible with the next one. Those callers allocate from the
    /// metadata store with [`crate`]-external monotonic state and pass it to
    /// [`Indexer::publish_manifest_at`].
    pub fn publish_manifest(&self) -> AppResult<IndexManifest> {
        let next = self.visible_generation() + 1;
        self.publish_manifest_at(next)
    }

    /// Provenance of a `doc_id`: `Overlay` if a non-tombstone overlay copy
    /// exists, otherwise `Snapshot`.
    pub fn provenance_of(&self, doc_id: &str) -> Provenance {
        let _ = self.overlay_reader.reload();
        let searcher = self.overlay_reader.searcher();
        let term = Term::from_field_text(self.fields.doc_id, doc_id);
        let query = tantivy::query::TermQuery::new(term, tantivy::schema::IndexRecordOption::Basic);
        match searcher.search(&query, &DocSetCollector) {
            Ok(addrs) if !addrs.is_empty() => Provenance::Overlay,
            _ => Provenance::Snapshot,
        }
    }
}

impl Candidate {
    /// Rebuild an `IndexDoc` from a stored overlay document for folding.
    fn to_index_doc(&self, generation: u64) -> IndexDoc {
        IndexDoc {
            doc_id: self.doc_id.clone(),
            version: self.version,
            resource_id: self.resource_id.clone(),
            work_id: self.work_id.clone(),
            content_id: self.content_id.clone(),
            url: self.url.clone(),
            title: self.title.clone(),
            headings: String::new(),
            body: self.body.clone(),
            entities: Vec::new(),
            source_id: self.source_id.clone(),
            doc_type: "html".to_string(),
            language: "en".to_string(),
            published_at: self.published_at,
            modified_at: self.modified_at,
            fetched_at: self.fetched_at,
            visible_generation: generation,
            tombstone: self.tombstone != 0,
        }
    }
}

// -- helpers ---------------------------------------------------------------

/// Read a FAST (columnar) u64 field for one document. These fields are not in
/// the doc store, so they must come from the segment's fast-field reader.
fn fast_u64(searcher: &Searcher, addr: DocAddress, name: &str) -> u64 {
    searcher
        .segment_reader(addr.segment_ord)
        .fast_fields()
        .u64(name)
        .ok()
        .and_then(|col| col.first(addr.doc_id))
        .unwrap_or(0)
}

/// Map any `Display` error from tantivy onto `AppError::Index`.
trait IxExt<T> {
    fn ix(self) -> AppResult<T>;
}

impl<T, E: std::fmt::Display> IxExt<T> for Result<T, E> {
    fn ix(self) -> AppResult<T> {
        self.map_err(|e| AppError::Index(e.to_string()))
    }
}

fn schema_hash(schema: &Schema) -> String {
    use sha2::{Digest, Sha256};
    let json = serde_json::to_string(schema).unwrap_or_default();
    let mut h = Sha256::new();
    h.update(json.as_bytes());
    hex::encode(h.finalize())
}

/// Open an existing index (asserting the frozen schema hash) or create it.
fn open_or_create(dir: &Path, schema: &Schema) -> AppResult<Index> {
    let expected = schema_hash(schema);
    if dir.join("meta.json").exists() {
        let index = Index::open_in_dir(dir).ix()?;
        // 1) persisted hash written by us at creation time.
        if let Ok(stored) = std::fs::read_to_string(dir.join(SCHEMA_HASH_FILE)) {
            if stored.trim() != expected {
                return Err(AppError::Index(format!(
                    "schema hash mismatch in {}: found {} expected {}",
                    dir.display(),
                    stored.trim(),
                    expected
                )));
            }
        }
        // 2) hash of the schema Tantivy actually loaded from meta.json.
        let actual = schema_hash(&index.schema());
        if actual != expected {
            return Err(AppError::Index(format!(
                "tantivy schema mismatch in {}: hash {} != expected {}",
                dir.display(),
                actual,
                expected
            )));
        }
        Ok(index)
    } else {
        std::fs::create_dir_all(dir)?;
        let index = Index::create_in_dir(dir, schema.clone()).ix()?;
        std::fs::write(dir.join(SCHEMA_HASH_FILE), format!("{expected}\n"))?;
        Ok(index)
    }
}

fn manifest_path(base_dir: &Path) -> PathBuf {
    base_dir.join(MANIFEST_FILE)
}

fn load_manifest(base_dir: &Path) -> AppResult<Option<IndexManifest>> {
    let p = manifest_path(base_dir);
    if !p.exists() {
        return Ok(None);
    }
    let bytes = std::fs::read(&p)?;
    Ok(Some(serde_json::from_slice(&bytes)?))
}

fn write_manifest(base_dir: &Path, m: &IndexManifest) -> AppResult<()> {
    let bytes = serde_json::to_vec_pretty(m)?;
    // Write to a temp file then rename for atomicity.
    let tmp = base_dir.join(format!("{MANIFEST_FILE}.tmp"));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, manifest_path(base_dir))?;
    Ok(())
}

fn make_snippet(body: &str) -> String {
    const MAX: usize = 200;
    let trimmed = body.trim();
    if trimmed.chars().count() <= MAX {
        return trimmed.to_string();
    }
    let mut s: String = trimmed.chars().take(MAX).collect();
    s.push('…');
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn tmp() -> (TempDir, Indexer) {
        let dir = TempDir::new().unwrap();
        let idx = Indexer::open(IndexerConfig::new(dir.path().join("tantivy"))).unwrap();
        (dir, idx)
    }

    fn doc(id: &str, title: &str, body: &str) -> IndexDoc {
        IndexDoc::new(id, format!("res_{id}"), format!("https://example.com/{id}"), title, body)
            .with_version(1)
    }

    #[test]
    fn a_snippet_carries_no_markup() {
        // The snippet contract is plain text. Tantivy's `.to_html()` wraps matches in <b>, and a
        // text-rendering client shows the tags literally — observed live as
        // "… DAST <b>winter</b> update 2025 …".
        let (_t, idx) = tmp();
        idx.index_batch(vec![doc(
            "d1",
            "Winter update",
            "the winter release notes describe the winter changes in detail",
        )])
        .unwrap();
        let hits = idx.search("winter", 10).unwrap();
        assert_eq!(hits.len(), 1);
        let snippet = &hits[0].snippet;
        assert!(!snippet.contains("<b>"), "no highlight markup: {snippet:?}");
        assert!(!snippet.contains("</b>"), "no highlight markup: {snippet:?}");
        assert!(!snippet.contains('<'), "no angle brackets at all: {snippet:?}");
        assert!(snippet.to_lowercase().contains("winter"), "the text is still there: {snippet:?}");
    }

    #[test]
    fn structured_fields_are_searchable_and_phrase_slop_is_honoured() {
        // Two capabilities the query pipeline depends on:
        //  1. `entities` and `headings` (where the organizing model's key entities, summary and
        //     topics land) are searched. They were in the schema but absent from the parser's field
        //     list, so a page the model tagged could not be found by that tag unless the prose
        //     happened to repeat it — the structuring had no retrieval effect at all.
        //  2. phrase proximity works. Tantivy's parser already honours `"a b"~N`, which is exactly
        //     the syntax an LLM query planner emits, so the parser needs no change for it.
        let (_t, idx) = tmp();
        let mut d = doc("d1", "A page about storage", "the prose never names the topic at all");
        d.entities = vec!["zetaquokka".to_string(), "paged attention".to_string()];
        d.headings = "memory complexity of attention".to_string();
        idx.index_batch(vec![d]).unwrap();

        assert_eq!(idx.search("zetaquokka", 10).unwrap().len(), 1, "entities must be searchable");
        assert_eq!(
            idx.search("memory complexity", 10).unwrap().len(),
            1,
            "headings must be searchable"
        );
        // Phrase with slop: adjacent, so it matches.
        assert_eq!(idx.search("\"memory complexity\"~3", 10).unwrap().len(), 1, "slop phrase matches");
        // Same words, closer slop than the gap between them: no match.
        assert!(
            idx.search("\"memory storage\"~1", 10).unwrap().is_empty(),
            "slop must not match across a gap"
        );
        // Control: a term in no field must not match.
        assert!(idx.search("unrelatedquokka", 10).unwrap().is_empty());
    }

    #[test]
    fn index_and_search_three_docs() {
        let (_t, idx) = tmp();
        idx.index_batch(vec![
            doc("d1", "Rust async runtimes", "tokio is an asynchronous runtime for rust"),
            doc("d2", "Python asyncio", "asyncio is a library to write concurrent code using async"),
            doc("d3", "Cooking pasta", "boil water and add salt before the pasta"),
        ])
        .unwrap();

        let hits = idx.search("asynchronous runtime", 10).unwrap();
        assert_eq!(hits.len(), 1, "only the rust doc mentions asynchronous runtime");
        assert_eq!(hits[0].doc_id, "d1");
        assert_eq!(hits[0].provenance, Provenance::Snapshot);
        assert_eq!(hits[0].lane, "lexical_base");
        assert!(!hits[0].signals.is_empty(), "every result carries a signal");

        // broader term matches two docs
        let hits = idx.search("async", 10).unwrap();
        assert_eq!(hits.len(), 2);
        assert!(hits.iter().all(|h| h.doc_id == "d1" || h.doc_id == "d2"));
    }

    #[test]
    fn snippet_is_excerpted_around_the_match_not_the_page_head() {
        let (_t, idx) = tmp();
        // Leading boilerplate, the matched term far into the body.
        let filler = "nav home about contact ".repeat(120);
        let body = format!("{filler} the transformer architecture dominates modern language modelling");
        idx.index_batch(vec![doc("d1", "Transformers", &body)])
            .unwrap();

        let hits = idx.search("transformer", 5).unwrap();
        assert_eq!(hits.len(), 1);
        let snippet = &hits[0].snippet;
        assert!(
            snippet.to_lowercase().contains("transformer"),
            "snippet must contain the matched term: {snippet}"
        );
        // The head fallback would be the first 200 chars of the body, which
        // here contain no occurrence of the term at all.
        assert_ne!(
            snippet,
            &make_snippet(&body),
            "snippet fell back to the page head instead of the match window"
        );
        assert!(
            snippet.chars().count() <= SNIPPET_CHARS + 32,
            "fragment must respect the character cap: {} chars",
            snippet.chars().count()
        );
    }

    #[test]
    fn doc_text_returns_stored_body_and_none_for_unknown() {
        let (_t, idx) = tmp();
        let body = "concrete detail about retrieval augmented generation pipelines";
        idx.index_batch(vec![doc("d1", "RAG", body)]).unwrap();

        assert_eq!(idx.doc_text("d1").unwrap().as_deref(), Some(body));
        assert_eq!(idx.doc_text("nope").unwrap(), None);

        // A newer overlay version wins, and a tombstone reads as absent.
        idx.index_overlay(
            IndexDoc::new("d1", "res_d1", "https://example.com/d1", "RAG", "newer body text")
                .with_version(2),
        )
        .unwrap();
        assert_eq!(idx.doc_text("d1").unwrap().as_deref(), Some("newer body text"));

        idx.delete("d1").unwrap();
        assert_eq!(idx.doc_text("d1").unwrap(), None);
    }

    #[test]
    fn overlay_newer_version_shadows_base() {
        let (_t, idx) = tmp();
        idx.index_batch(vec![doc("d1", "Old title", "old body content")])
            .unwrap();
        // Same doc_id, newer version, goes to overlay.
        idx.index_overlay(
            IndexDoc::new("d1", "res_d1", "https://example.com/d1", "New title", "freshbody zebra")
                .with_version(2),
        )
        .unwrap();

        let hits = idx.search("zebra", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].doc_id, "d1");
        assert_eq!(hits[0].version, 2);
        assert_eq!(hits[0].title, "New title");
        assert_eq!(hits[0].provenance, Provenance::Overlay);
        assert_eq!(hits[0].lane, "lexical_overlay");

        // Searching the old-only term must not resurrect the shadowed version.
        let hits = idx.search("oldbodyterm", 10).unwrap();
        assert!(hits.is_empty(), "shadowed base version must not be returned");
    }

    #[test]
    fn index_overlay_batch_commits_once_and_gates_until_publish() {
        let (_t, idx) = tmp();
        idx.index_batch(vec![doc("d1", "Base title", "base sharedterm body")])
            .unwrap();
        idx.publish_manifest().unwrap();

        // Two docs in one batch, both claiming the generation the next publish
        // will create (exactly what the crawl's flush does).
        let generation = idx.visible_generation() + 1;
        let mut shadow = doc("d1", "Overlay title", "overlay sharedterm zebra").with_version(2);
        shadow.visible_generation = generation;
        let mut fresh = doc("d2", "Fresh title", "fresh zebra body");
        fresh.visible_generation = generation;
        idx.index_overlay_batch(vec![shadow, fresh]).unwrap();

        // Committed in one call, but gated until the manifest reaches the batch
        // generation — no doc is exposed ahead of its publish.
        assert!(
            idx.search("zebra", 10).unwrap().is_empty(),
            "batched docs must stay gated until the manifest is published"
        );

        idx.publish_manifest().unwrap();
        let mut hits = idx.search("zebra", 10).unwrap();
        hits.sort_by(|a, b| a.doc_id.cmp(&b.doc_id));
        assert_eq!(hits.len(), 2, "both batched docs visible after publish");
        assert_eq!(hits[0].doc_id, "d1");
        assert_eq!(hits[0].title, "Overlay title");
        assert_eq!(hits[0].provenance, Provenance::Overlay);
        // A term present in both copies resolves to the newer overlay version only.
        let shared = idx.search("sharedterm", 10).unwrap();
        assert_eq!(shared.len(), 1, "the overlay copy shadows the base copy");
        assert_eq!(shared[0].version, 2);
        assert_eq!(shared[0].provenance, Provenance::Overlay);

        // An empty batch is a no-op, not a commit.
        let gen_before = idx.visible_generation();
        idx.index_overlay_batch(Vec::new()).unwrap();
        assert_eq!(idx.visible_generation(), gen_before);
    }

    #[test]
    fn tombstone_delete_is_not_returned() {
        let (_t, idx) = tmp();
        idx.index_batch(vec![doc("d1", "Delete me", "unique deletephrase here")])
            .unwrap();
        assert_eq!(idx.search("deletephrase", 10).unwrap().len(), 1);

        idx.delete("d1").unwrap();
        assert!(
            idx.search("deletephrase", 10).unwrap().is_empty(),
            "tombstoned doc must not be returned"
        );
        // A different doc is unaffected.
        idx.index_batch(vec![doc("d2", "Keep me", "keep unique otherphrase")])
            .unwrap();
        assert_eq!(idx.search("otherphrase", 10).unwrap().len(), 1);
    }

    #[test]
    fn fold_moves_docs_and_bumps_generation() {
        let (_t, idx) = tmp();
        let m0 = idx.publish_manifest().unwrap();
        assert_eq!(m0.generation, 1);

        idx.index_overlay(doc("d1", "Fold me", "overlay only phrase")).unwrap();
        assert_eq!(idx.search("phrase", 10).unwrap()[0].provenance, Provenance::Overlay);

        let before = idx.overlay_watermark();
        let m = idx.fold_overlay_into_base().unwrap();
        assert_eq!(m.generation, before + 1, "fold bumps the generation");
        assert!(m.doc_count >= 1);

        // Doc now lives in base.
        let hits = idx.search("phrase", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].provenance, Provenance::Snapshot);

        // Overlay was cleared.
        let overlay_searcher = idx.overlay_reader.searcher();
        let addrs = overlay_searcher.search(&AllQuery, &DocSetCollector).unwrap();
        assert!(addrs.is_empty(), "overlay must be empty after fold");
    }

    #[test]
    fn persistence_across_reopen() {
        let dir = TempDir::new().unwrap();
        let cfg = IndexerConfig::new(dir.path().join("tantivy"));
        {
            let idx = Indexer::open(cfg.clone()).unwrap();
            idx.index_batch(vec![doc("d1", "Persistent title", "persisted unique phrase")])
                .unwrap();
            idx.publish_manifest().unwrap();
        }
        // New "process": reopen from the same dirs.
        let idx2 = Indexer::open(cfg).unwrap();
        let hits = idx2.search("persisted unique phrase", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].doc_id, "d1");
        assert!(idx2.visible_generation() >= 1, "manifest persisted across reopen");
    }

    #[test]
    fn doc_count_is_exact_across_replace_delete_and_duplicate() {
        // The published count is what the manifest reports, and it used to be produced by scanning
        // the whole index. It must stay exact now that it is computed from segment metadata plus a
        // bounded overlay pass: a replacement is one document, a tombstone is none, and re-adding
        // an existing `doc_id` must not inflate it.
        let (_t, idx) = tmp();
        idx.index_batch(vec![
            doc("d1", "One", "alpha body"),
            doc("d2", "Two", "beta body"),
        ])
        .unwrap();
        assert_eq!(idx.publish_manifest().unwrap().doc_count, 2);

        // Same doc_id, newer version: still one live document.
        idx.index_overlay(
            IndexDoc::new(
                "d1",
                "res_d1",
                "https://example.com/d1",
                "One v2",
                "alpha v2",
            )
            .with_version(2),
        )
        .unwrap();
        assert_eq!(
            idx.publish_manifest().unwrap().doc_count,
            2,
            "a replacement is still one live document"
        );

        // Tombstone: not counted.
        idx.delete("d2").unwrap();
        assert_eq!(
            idx.publish_manifest().unwrap().doc_count,
            1,
            "tombstoned documents are not live"
        );

        // Re-adding an existing doc_id must not increase the count.
        idx.index_batch(vec![doc("d1", "One v3", "alpha v3")])
            .unwrap();
        assert_eq!(
            idx.publish_manifest().unwrap().doc_count,
            1,
            "a duplicate add is not a new document"
        );
    }

    #[test]
    fn manifest_getter_none_before_publish_then_published() {
        let (_t, idx) = tmp();
        // Nothing published yet: the honest read-side state is `None`.
        assert!(
            idx.manifest().unwrap().is_none(),
            "no manifest before the first publish"
        );

        idx.index_batch(vec![doc("d1", "Title", "some body text")])
            .unwrap();
        let published = idx.publish_manifest().unwrap();
        let read = idx.manifest().unwrap().expect("manifest after publish");
        assert_eq!(read.manifest_id, published.manifest_id);
        assert_eq!(read.generation, published.generation);
        assert_eq!(read.doc_count, published.doc_count);
        assert_eq!(read.base_segments, published.base_segments);
        // The getter agrees with the watermark accessors.
        assert_eq!(read.generation, idx.visible_generation());
    }

    #[test]
    fn manifest_getter_survives_reopen() {
        let dir = TempDir::new().unwrap();
        let cfg = IndexerConfig::new(dir.path().join("tantivy"));
        {
            let idx = Indexer::open(cfg.clone()).unwrap();
            assert!(idx.manifest().unwrap().is_none());
            idx.publish_manifest().unwrap();
        }
        let idx2 = Indexer::open(cfg).unwrap();
        let m = idx2.manifest().unwrap().expect("manifest persisted across reopen");
        assert!(m.generation >= 1);
    }

    #[test]
    fn watermark_gates_not_yet_visible_docs() {
        let (_t, idx) = tmp();
        idx.publish_manifest().unwrap(); // generation 1
        // A doc claiming a future generation must be gated until published.
        let mut d = doc("future", "Future doc", "future phrase here");
        d.visible_generation = 99;
        idx.index_batch(vec![d]).unwrap();
        assert!(idx.search("future phrase", 10).unwrap().is_empty());
        // A normal doc (generation 0 <= watermark) is visible.
        idx.index_batch(vec![doc("now", "Now doc", "present phrase here")]).unwrap();
        assert_eq!(idx.search("present phrase", 10).unwrap().len(), 1);
    }

    #[test]
    fn all_docs_returns_every_live_doc_with_stored_fields() {
        let (_t, idx) = tmp();
        idx.index_batch(vec![
            doc("d1", "Rust async", "tokio runtime body"),
            doc("d2", "Python asyncio", "asyncio body"),
            doc("d3", "Cooking", "pasta body"),
        ])
        .unwrap();
        idx.publish_manifest().unwrap();

        let docs = idx.all_docs().unwrap();
        assert_eq!(docs.len(), 3, "all three live docs exported");
        // Deterministic (doc_id-sorted) order: two runs must agree byte-for-byte.
        let ids: Vec<&str> = docs.iter().map(|d| d.doc_id.as_str()).collect();
        assert_eq!(ids, vec!["d1", "d2", "d3"]);
        // The export source is the STOREd fields, not the blob store.
        let d1 = docs.iter().find(|d| d.doc_id == "d1").unwrap();
        assert_eq!(d1.title, "Rust async");
        assert_eq!(d1.body, "tokio runtime body");
        assert_eq!(d1.url, "https://example.com/d1");
        // `index_batch` stores the doc's own generation (0 by default); it must
        // never be reportable above the published watermark.
        assert!(d1.visible_generation <= idx.visible_generation());

        // Idempotent: a second export equals the first.
        assert_eq!(idx.all_docs().unwrap(), docs);
    }

    #[test]
    fn all_docs_drops_tombstoned_and_prefers_the_newer_overlay_copy() {
        let (_t, idx) = tmp();
        idx.index_batch(vec![
            doc("d1", "Base title", "base body oldterm"),
            doc("d2", "Doomed", "doomed body"),
        ])
        .unwrap();
        idx.publish_manifest().unwrap();

        // A newer overlay copy of d1 shadows the base copy...
        idx.index_overlay(
            IndexDoc::new(
                "d1",
                "res_d1",
                "https://example.com/d1",
                "Overlay title",
                "overlay body newterm",
            )
            .with_version(2),
        )
        .unwrap();
        // ...and a tombstone removes d2 entirely.
        idx.index_overlay(IndexDoc::tombstone("d2", 2)).unwrap();

        let docs = idx.all_docs().unwrap();
        assert_eq!(docs.len(), 1, "d2 is tombstoned; d1 collapses to one doc");
        assert_eq!(docs[0].doc_id, "d1");
        assert_eq!(docs[0].body, "overlay body newterm", "overlay copy wins");
        assert_eq!(docs[0].title, "Overlay title");
    }

    #[test]
    fn schema_mismatch_is_detected() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().join("base");
        std::fs::create_dir_all(&base).unwrap();
        // Create an index with a deliberately different schema.
        let mut b = tantivy::schema::Schema::builder();
        b.add_text_field("totally_different", tantivy::schema::STRING);
        tantivy::Index::create_in_dir(&base, b.build()).unwrap();

        // Built from the constructor rather than a literal, so a new config field cannot leave
        // this test uncompilable (which has happened twice in this codebase already).
        let cfg = IndexerConfig {
            base_dir: base,
            ..IndexerConfig::new(dir.path().join("tantivy"))
        };
        let err = match Indexer::open(cfg) {
            Ok(_) => panic!("expected a schema-mismatch error"),
            Err(e) => e,
        };
        match err {
            AppError::Index(msg) => assert!(msg.contains("mismatch"), "got: {msg}"),
            other => panic!("expected schema mismatch, got {other:?}"),
        }
    }
}
