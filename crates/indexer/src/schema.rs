//! Tantivy schema (spec §6, v2 §16). OWNER: indexer agent.
//!
//! Tantivy enforces a 1:1 field<->index relationship, so boosted/concatenated
//! fields are sent explicitly rather than computed by the engine.
//!
//! # The index model: addresses and short evidence, never the article
//!
//! This index stores a document's *identity*, the lexical evidence a query can match against
//! (`title`, `headings`, `entities`, `excerpt`), and the timestamps a result row needs. It does
//! **not** store the article. Measured for the same corpus: the full-text shape was 16.6 GB of
//! Tantivy against a 1–2 GB target, because `body` was `TEXT | STORED` and averaged 10.6 KB per
//! document (max 1.8 MB) — the whole page indexed *and* a second copy stored.
//!
//! The article survives as two derived artifacts, neither of which is a copy of the page:
//!
//! * `excerpt` — ~500 chars of the extracted text, `TEXT | STORED`. It is the lexical evidence
//!   for body prose and the snippet source, so a result row and `/read`-style callers keep
//!   working without a body store.
//! * the semantic layers — dense vectors (`vectors.bin`), sparse feature postings
//!   (`doc_feature` / the sparse index), and the concept matrix (`doc_concept`,
//!   `doc_concept_dim`). v2 §5/§6/§9: associations, not text.
//!
//! Full text remains on disk as `text/<hash>.txt` (the ingest source) and is read on demand by
//! `Indexer::doc_text` and by batch jobs (`Pipeline::all_docs`). Deleting it is a separate,
//! gated operation (`docs/PLAN.md` Phase 2) because it cannot be undone.
//!
//! Consequences to keep in mind: every schema change makes existing indices unopenable (the
//! `_schema_hash` guard is deliberate), so a schema edit ships **with** a rebuild in one
//! operation. See `docs/ARCHITECTURE.md` §7.2.

use tantivy::schema::*;

/// Build the index schema exactly as frozen in spec §6.
pub fn build_schema() -> Schema {
    let mut b = Schema::builder();

    b.add_text_field("doc_id", STRING | STORED); // delete term
    b.add_u64_field("version", INDEXED | FAST);
    b.add_text_field("resource_id", STRING | STORED);
    b.add_text_field("work_id", STRING | STORED);
    b.add_text_field("content_id", STRING | STORED);
    b.add_text_field("url", TEXT | STORED);
    b.add_text_field("title", TEXT | STORED);
    b.add_text_field("title_boost", TEXT);
    // STORED so a result row can carry the organizing model's headings without a second read.
    b.add_text_field("headings", TEXT | STORED);
    // The only article text in the index: ~500 chars, indexed for BM25 and stored for snippets.
    b.add_text_field("excerpt", TEXT | STORED);
    b.add_text_field("entities", TEXT | STORED);
    b.add_text_field("source_id", STRING | STORED);
    // STORED so faceting and export read them back; they used to be indexed-only and silently
    // came back empty from `all_docs`.
    b.add_text_field("doc_type", STRING | STORED);
    b.add_text_field("language", STRING | STORED);
    b.add_i64_field("published_at", INDEXED | FAST | STORED);
    b.add_i64_field("modified_at", FAST | STORED);
    b.add_i64_field("fetched_at", FAST | STORED);
    b.add_u64_field("visible_generation", FAST);
    b.add_u64_field("tombstone", FAST);

    b.build()
}