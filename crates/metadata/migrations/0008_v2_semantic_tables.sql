-- 0008: the three tables PLAN-v2.md §3 names as "Schema still to add".
--
-- These are the structural half of v2's Phase 3/4/6. They were specified but never created:
-- `doc_concept` (0005) holds one scalar `weight` per (document, concept), while v2 §6 requires a
-- *vector* R(d,c) = [r1, r2, ..., rm] so that the same article can carry a strong direct
-- association with neurochemistry and a weaker contextual one with romantic attachment, and the
-- same article can then rank differently for "chemical processes involved in romance" and
-- "hormonal regulation" without being reindexed.
--
-- Why separate tables rather than columns on `doc_concept`: v2 §6 is explicit that the set of
-- dimensions is not fixed — "the relationship dimensions should be learned or explicitly
-- validated. They must not merely be assumed to exist because several arbitrary output heads were
-- created." A column per dimension would freeze today's guesses into the schema; a row per
-- (document, concept, dimension) lets a new model add a dimension without a migration.

PRAGMA foreign_keys = ON;

-- ---------------------------------------------------------------------------
-- feature: the vocabulary of the sparse neural representation (v2 §5.2, §9)
-- ---------------------------------------------------------------------------
-- A `feature_id` is meaningless without the model that produced it: feature 4096 means one thing
-- for model A and something unrelated for model B, and a re-trained encoder renumbers everything.
-- Storing the model/version/space alongside the id is what makes a stored posting interpretable
-- after the model changes, and lets an old and a new encoder coexist during a rollout instead of
-- silently mixing two vocabularies in one postings list.
CREATE TABLE IF NOT EXISTS feature (
  feature_id     INTEGER NOT NULL,
  model_id       TEXT NOT NULL,
  version        INTEGER NOT NULL,
  space_id       TEXT NOT NULL,
  -- Human-facing trace of what the feature learned, when it is interpretable at all. Sparse
  -- autoencoder features often are not; NULL is honest, an invented label is not.
  label          TEXT,
  created_at     INTEGER NOT NULL,
  PRIMARY KEY (model_id, version, space_id, feature_id)
);

-- ---------------------------------------------------------------------------
-- doc_feature: the forward index (v2 §5.2, §9, §18)
-- ---------------------------------------------------------------------------
-- ArticleID -> active features, with activation weight. v2 §9 requires the forward representation
-- be retained "to support efficient updates and reranking": the inverted postings answer
-- feature -> articles, but a reranker holding a candidate article needs its features without
-- scanning every postings list, and an update needs to know which postings to retract.
--
-- Only active features are stored (TopK of the encoder's output), so a row count of
-- documents × active features is the *whole* matrix, not a dense one. v2 §16: "The architecture
-- should never allocate a complete article-by-concept matrix. Store only activated features."
CREATE TABLE IF NOT EXISTS doc_feature (
  resource_id    TEXT NOT NULL REFERENCES resource(resource_id) ON DELETE CASCADE,
  feature_id     INTEGER NOT NULL,
  model_id       TEXT NOT NULL,
  version        INTEGER NOT NULL,
  space_id       TEXT NOT NULL,
  -- Activation after TopK. REAL rather than INTEGER because the quantisation step belongs to the
  -- postings encoding in the sparse index, not to this table: this is the authoritative value, and
  -- a lossy copy should never be the only one kept.
  activation     REAL NOT NULL,
  created_at     INTEGER NOT NULL,
  PRIMARY KEY (resource_id, model_id, version, space_id, feature_id)
);

-- The reverse direction, for building postings and for "which documents activated feature F".
CREATE INDEX IF NOT EXISTS idx_doc_feature_feature
  ON doc_feature(model_id, version, space_id, feature_id);

-- ---------------------------------------------------------------------------
-- doc_concept_dim: the multidimensional association R(d,c) (v2 §6)
-- ---------------------------------------------------------------------------
-- One row per (document, concept, dimension). The dimension is a string key rather than an ordinal
-- because v2 §6 names meanings ("direct topical relevance", "contextual or indirect relevance",
-- "relevance to mechanisms or processes", "relevance to a particular interpretation") and a
-- trained model may discover dimensions that do not map onto those names; a name survives that, an
-- integer index does not.
--
-- These rows are *additional detail* on an association that `doc_concept` already records. A
-- document with no dimensional breakdown still has its scalar weight there, so this table can be
-- populated incrementally by a better teacher without invalidating existing retrieval.
--
-- Evidence quality, factual support and model confidence live here as their own dimensions rather
-- than being folded into relevance, because v2 §6 requires them recorded "separately when they
-- cannot be represented by reliably trained and calibrated dimensions".
CREATE TABLE IF NOT EXISTS doc_concept_dim (
  resource_id    TEXT NOT NULL REFERENCES resource(resource_id) ON DELETE CASCADE,
  concept_id     TEXT NOT NULL REFERENCES concept(concept_id) ON DELETE CASCADE,
  dim            TEXT NOT NULL,
  value          REAL NOT NULL,
  model_version  TEXT,
  created_at     INTEGER NOT NULL,
  updated_at     INTEGER NOT NULL,
  PRIMARY KEY (resource_id, concept_id, dim, model_version)
);

-- Reading a concept's strength along one dimension across documents: the query path when a query
-- emphasises "mechanism" over "direct topic".
CREATE INDEX IF NOT EXISTS idx_doc_concept_dim_concept
  ON doc_concept_dim(concept_id, dim, value DESC);
