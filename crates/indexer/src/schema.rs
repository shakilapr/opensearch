//! Tantivy schema (spec §6). OWNER: indexer agent.
//!
//! Tantivy enforces a 1:1 field<->index relationship, so boosted/concatenated
//! fields are sent explicitly rather than computed by the engine.

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
    b.add_text_field("headings", TEXT);
    b.add_text_field("body", TEXT | STORED);
    b.add_text_field("body_boost", TEXT);
    b.add_text_field("entities", TEXT | STORED);
    b.add_text_field("source_id", STRING | STORED);
    b.add_text_field("doc_type", STRING);
    b.add_text_field("language", STRING);
    b.add_i64_field("published_at", INDEXED | FAST | STORED);
    b.add_i64_field("modified_at", FAST | STORED);
    b.add_i64_field("fetched_at", FAST | STORED);
    b.add_u64_field("visible_generation", FAST);
    b.add_u64_field("tombstone", FAST);

    b.build()
}
