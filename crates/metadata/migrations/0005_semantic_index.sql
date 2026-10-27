-- Migration 0005: the semantic index — concepts, associations, passages, and web links.
--
-- Implements the identity + semantic-knowledge layer from
-- `docs/SEMANTIC_INDEX_ARCHITECTURE.md`. Two rules shape this schema:
--
--   * The semantic index points at **stable ids** (`resource_id`, `passage_id`,
--     `concept_id`), never at file paths, vector offsets, or model positions. The
--     models, embeddings, and postings can therefore be rebuilt or replaced
--     without breaking document identity.
--   * Every association carries its **provenance**: whether it is direct evidence
--     from the document or inferred through the concept graph, which passage
--     supports it, and which model version produced it. Direct and inferred
--     scores must never be merged into one number, or a later model reinforces
--     the earlier one's mistakes.
--
-- SQLite remains the source of truth. Concept-to-document postings can be
-- materialized later from `doc_concept`; this migration stores the facts, not a
-- second copy of them.

PRAGMA foreign_keys = ON;

-- ---------------------------------------------------------------------------
-- Concepts: the vocabulary the index weighs documents against
-- ---------------------------------------------------------------------------
-- `kind` is descriptive, not a closed enum: `topic`, `entity`, `product`,
-- `field`, `term`. `status` lets a concept be retired without deleting the
-- associations that cite it (`active`, `merged`, `retired`).
CREATE TABLE IF NOT EXISTS concept (
  concept_id    TEXT PRIMARY KEY,
  name          TEXT NOT NULL,
  description   TEXT,
  kind          TEXT NOT NULL DEFAULT 'topic',
  status        TEXT NOT NULL DEFAULT 'active',
  created_at    INTEGER NOT NULL,
  updated_at    INTEGER NOT NULL
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_concept_name ON concept(name);
CREATE INDEX IF NOT EXISTS idx_concept_status ON concept(status);

-- Surface forms that resolve to a concept, across languages and spellings.
-- `alias` is the primary key component, so one string cannot point at two
-- concepts; a genuine ambiguity is modelled as two concepts plus a `related`
-- relation, not as one alias with two owners.
CREATE TABLE IF NOT EXISTS concept_alias (
  concept_id  TEXT NOT NULL REFERENCES concept(concept_id) ON DELETE CASCADE,
  alias       TEXT NOT NULL,
  lang        TEXT,
  created_at  INTEGER NOT NULL,
  PRIMARY KEY (concept_id, alias)
);

CREATE INDEX IF NOT EXISTS idx_concept_alias_alias ON concept_alias(alias);

-- Typed edges between concepts, following the SKOS vocabulary (equivalent,
-- broader, narrower, related) plus free-form `typed` relations carried in
-- `relation` (e.g. `mechanism_of`, `causes`, `located_in`).
--
-- Direction is meaningful: `broader` is stored from narrow to broad, and the
-- inverse is derived on read rather than duplicated here.
CREATE TABLE IF NOT EXISTS concept_relation (
  src_concept_id  TEXT NOT NULL REFERENCES concept(concept_id) ON DELETE CASCADE,
  dst_concept_id  TEXT NOT NULL REFERENCES concept(concept_id) ON DELETE CASCADE,
  relation        TEXT NOT NULL,
  strength        REAL,
  evidence        TEXT,
  model_version   TEXT,
  created_at      INTEGER NOT NULL,
  PRIMARY KEY (src_concept_id, dst_concept_id, relation),
  CHECK (src_concept_id <> dst_concept_id)
);

CREATE INDEX IF NOT EXISTS idx_concept_relation_dst ON concept_relation(dst_concept_id, relation);

-- ---------------------------------------------------------------------------
-- Document → concept associations (the weighted links that answer queries)
-- ---------------------------------------------------------------------------
-- `relation_kind` separates the two kinds of claim the design requires:
--   `direct`   — the document itself states the association (with `passage_id`
--                or `evidence` naming where);
--   `inferred` — derived through `concept_relation` (with `via_concept_id`
--                naming the hop).
-- A retrieval score may use both, but must weigh them differently; keeping them
-- in separate rows is what makes `UPDATE ... SET weight` on a model upgrade
-- possible without losing the distinction.
--
-- The document identity is `resource_id`, which exists for every fetched page;
-- `work_id` is carried when known (a work may span several resources).
CREATE TABLE IF NOT EXISTS doc_concept (
  resource_id     TEXT NOT NULL REFERENCES resource(resource_id) ON DELETE CASCADE,
  concept_id      TEXT NOT NULL REFERENCES concept(concept_id) ON DELETE CASCADE,
  weight          REAL NOT NULL,
  relation_kind   TEXT NOT NULL DEFAULT 'direct',
  evidence        TEXT,
  passage_id      TEXT,
  via_concept_id  TEXT,
  model_version   TEXT,
  work_id         TEXT,
  created_at      INTEGER NOT NULL,
  updated_at      INTEGER NOT NULL,
  -- The key includes `relation_kind` so a document can hold BOTH a direct
  -- association and an inferred one for the same concept: they are separate
  -- claims with separate provenance, and a reader prefers `direct`.
  PRIMARY KEY (resource_id, concept_id, relation_kind),
  CHECK (weight >= 0.0 AND weight <= 1.0),
  CHECK (relation_kind IN ('direct','inferred'))
);

-- The important direction: concept -> documents, without a full-table scan.
CREATE INDEX IF NOT EXISTS idx_doc_concept_concept ON doc_concept(concept_id, weight DESC);
CREATE INDEX IF NOT EXISTS idx_doc_concept_work ON doc_concept(work_id);

-- ---------------------------------------------------------------------------
-- Passages: where inside a document an association was observed
-- ---------------------------------------------------------------------------
-- `heading_path` is the H1/H2/H3 chain the passage sits under, kept because it
-- is often the cheapest topic signal a page offers and it survives after the
-- raw HTML is deleted.
CREATE TABLE IF NOT EXISTS passage (
  passage_id    TEXT PRIMARY KEY,
  resource_id   TEXT NOT NULL REFERENCES resource(resource_id) ON DELETE CASCADE,
  content_id    TEXT,
  ordinal       INTEGER NOT NULL,
  start_off     INTEGER NOT NULL,
  end_off       INTEGER NOT NULL,
  heading_path  TEXT,
  created_at    INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_passage_resource ON passage(resource_id, ordinal);

CREATE TABLE IF NOT EXISTS passage_concept (
  passage_id    TEXT NOT NULL REFERENCES passage(passage_id) ON DELETE CASCADE,
  concept_id    TEXT NOT NULL REFERENCES concept(concept_id) ON DELETE CASCADE,
  weight        REAL NOT NULL,
  model_version TEXT,
  created_at    INTEGER NOT NULL,
  PRIMARY KEY (passage_id, concept_id),
  CHECK (weight >= 0.0 AND weight <= 1.0)
);

CREATE INDEX IF NOT EXISTS idx_passage_concept_concept ON passage_concept(concept_id, weight DESC);

-- ---------------------------------------------------------------------------
-- Web links: the navigation graph, derived from raw HTML
-- ---------------------------------------------------------------------------
-- `dst_url` is the canonicalized target (a link may point at a URL that is not
-- yet a resource); `dst_resource_id` is filled once the target is known.
-- Capture order matters: this is derived from raw HTML and cannot be rebuilt
-- after the blobs are deleted without re-crawling.
CREATE TABLE IF NOT EXISTS web_link (
  src_resource_id  TEXT NOT NULL REFERENCES resource(resource_id) ON DELETE CASCADE,
  dst_url          TEXT NOT NULL,
  dst_resource_id  TEXT,
  rel              TEXT,
  anchor           TEXT,
  internal         INTEGER NOT NULL DEFAULT 0,
  created_at       INTEGER NOT NULL,
  PRIMARY KEY (src_resource_id, dst_url)
);

CREATE INDEX IF NOT EXISTS idx_web_link_dst ON web_link(dst_url);
CREATE INDEX IF NOT EXISTS idx_web_link_dst_resource ON web_link(dst_resource_id);

-- ---------------------------------------------------------------------------
-- Model registry: every inferred fact names the model that produced it
-- ---------------------------------------------------------------------------
-- One row per (model_id, version). `active` marks the version new inferences
-- should use; old versions stay resolvable so a rollback or a comparison does
-- not require re-running anything.
CREATE TABLE IF NOT EXISTS model_registry (
  model_id      TEXT NOT NULL,
  version       TEXT NOT NULL,
  kind          TEXT NOT NULL,
  status        TEXT NOT NULL DEFAULT 'active',
  trained_at    INTEGER,
  params_json   TEXT,
  notes         TEXT,
  created_at    INTEGER NOT NULL,
  PRIMARY KEY (model_id, version)
);

CREATE INDEX IF NOT EXISTS idx_model_registry_status ON model_registry(model_id, status);
