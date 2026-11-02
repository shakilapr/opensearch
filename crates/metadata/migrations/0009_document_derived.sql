-- 0009: keep the derived fields that cannot be recomputed after the fact.
--
-- The index model (docs/ARCHITECTURE-v2.md §5) deliberately does not store the article, so every
-- rebuild reads `text/<hash>.txt`. Extracted text is enough to rebuild the excerpt, the vectors and
-- the concept tags — but NOT the two fields the organising model produced at ingest:
--
--   * `headings`  — the H1/H2 chain (boosted 2.0 in the lexical index)
--   * `entities`  — the key entities (boosted 3.0)
--
-- Those live only in the `IndexDoc` the crawler writes, so before this table a rebuild silently
-- replaced a ranked index with one whose heading and entity fields were empty. The rebuild is
-- routine now (any schema change), which makes that an every-release regression, not a one-off.
--
-- This table is the smallest thing that fixes it: one row per document version, no duplication of
-- the article, and it survives `text/` being deleted someday because headings are not body text.
-- `excerpt` is deliberately NOT stored here: it is a pure function of the text (`excerpt_of`) and
-- storing it would be a second copy that can drift from the first.
CREATE TABLE IF NOT EXISTS document_derived (
  docver_id  TEXT PRIMARY KEY REFERENCES document_version(docver_id) ON DELETE CASCADE,
  -- Newline-joined H1/H2/H3 chain, exactly as the index stores it.
  headings   TEXT,
  -- Comma-joined, matching the index field's encoding (a space join would shred multi-word
  -- entities like "paged attention" and they could not be split back).
  entities   TEXT,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL
);