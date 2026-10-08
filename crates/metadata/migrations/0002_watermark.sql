-- Migration 0002: published watermark (spec §3 rule 3, §6).
--
-- The publish barrier between "written to the index" and "visible to readers":
-- a search returns a document version only when its Tantivy `visible_generation`
-- is <= the published watermark stored here.
--
-- Single-row table. Metadata owns the schema; the ingest publish path
-- (crates/ingest/src/watermark.rs) only reads/writes this table — it must NOT
-- create it lazily at runtime.

CREATE TABLE IF NOT EXISTS published_watermark (
  id           INTEGER PRIMARY KEY CHECK (id = 1),
  generation   INTEGER NOT NULL,
  published_at INTEGER NOT NULL
);
