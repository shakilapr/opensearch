-- Migration 0003: Stage-1 discovery / source / graph tables + resource freshness
-- (see docs/STAGE1_SPEC.md §"Metadata additions (Stage 1)").
--
-- Creates the persistence for discovery (sitemap, feed), the source-profile
-- registry, and the entity/concept graph, then adds freshness columns to the
-- existing `resource` table.
--
-- `resource` already has `first_seen` and `last_fetch` (0001), so the freshness
-- columns added here are kept distinct from them and are NOT duplicated.
--
-- The migration runner records this version in `schema_migration` and applies
-- it at most once, so the non-idempotent `ALTER TABLE ... ADD COLUMN` below is
-- safe: a replay is skipped by the ledger, not re-executed.

PRAGMA foreign_keys = ON;

-- ---------------------------------------------------------------------------
-- Discovery: sitemaps
-- ---------------------------------------------------------------------------
-- One row per sitemap document (a <urlset> or a <sitemapindex>). `source_url`
-- is the advertiser (robots.txt or the parent sitemap index); it is NULL for a
-- seed sitemap. `is_index` marks sitemapindex files whose children are other
-- sitemaps. Conditional-refresh validators (`etag`, `last_modified`) let the
-- discovery layer skip re-parsing on 304.
CREATE TABLE IF NOT EXISTS sitemap (
  sitemap_id      TEXT PRIMARY KEY,
  url             TEXT NOT NULL UNIQUE,
  source_url      TEXT,
  domain_id       TEXT REFERENCES domain(domain_id),
  is_index        INTEGER NOT NULL DEFAULT 0,
  status          TEXT NOT NULL DEFAULT 'NEW',
  url_count       INTEGER NOT NULL DEFAULT 0,
  etag            TEXT,
  last_modified   TEXT,
  fetched_at      INTEGER,
  created_at      INTEGER NOT NULL,
  updated_at      INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_sitemap_domain ON sitemap(domain_id);

-- ---------------------------------------------------------------------------
-- Discovery: feeds (RSS 2.0 / Atom 1.0)
-- ---------------------------------------------------------------------------
-- One row per feed document. `watermark` is an opaque per-feed cursor (e.g. the
-- newest entry GUID); `entry_count` counts discovered entries. Conditional
-- refresh validators mirror `sitemap`.
CREATE TABLE IF NOT EXISTS feed (
  feed_id         TEXT PRIMARY KEY,
  url             TEXT NOT NULL UNIQUE,
  domain_id       TEXT REFERENCES domain(domain_id),
  title           TEXT,
  feed_type       TEXT NOT NULL DEFAULT 'rss',
  status          TEXT NOT NULL DEFAULT 'NEW',
  entry_count     INTEGER NOT NULL DEFAULT 0,
  etag            TEXT,
  last_modified   TEXT,
  watermark       TEXT,
  fetched_at      INTEGER,
  created_at      INTEGER NOT NULL,
  updated_at      INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_feed_domain ON feed(domain_id);

-- ---------------------------------------------------------------------------
-- Source profiles (mirrors search-sources::SourceProfile, plain columns only)
-- ---------------------------------------------------------------------------
-- `topics`, `entities`, `url_patterns` are JSON-encoded TEXT arrays so this
-- crate keeps no dependency on the domain crates. `authority`, `change_rate`
-- and `js_probability` are the smoothed running estimates.
CREATE TABLE IF NOT EXISTS source_profile (
  source_id      TEXT PRIMARY KEY,
  domain         TEXT NOT NULL,
  topics         TEXT NOT NULL DEFAULT '[]',
  entities       TEXT NOT NULL DEFAULT '[]',
  url_patterns   TEXT NOT NULL DEFAULT '[]',
  authority      REAL NOT NULL DEFAULT 0.5,
  change_rate    REAL NOT NULL DEFAULT 0.0,
  js_probability REAL NOT NULL DEFAULT 0.0,
  fetch_count    INTEGER NOT NULL DEFAULT 0,
  error_count    INTEGER NOT NULL DEFAULT 0,
  created_at     INTEGER NOT NULL,
  updated_at     INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_source_profile_domain ON source_profile(domain);

-- ---------------------------------------------------------------------------
-- Entity graph
-- ---------------------------------------------------------------------------
-- `entity` holds canonical nodes; `entity_alias` holds the alternate surface
-- forms resolved to a node; `graph_edge` holds typed, weighted edges between
-- nodes (AliasOf / RelatedTo / Cites / AuthoredBy / PublishedBy).
CREATE TABLE IF NOT EXISTS entity (
  entity_id   TEXT PRIMARY KEY,
  name        TEXT NOT NULL,
  kind        TEXT NOT NULL DEFAULT 'other',
  created_at  INTEGER NOT NULL,
  updated_at  INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_entity_name ON entity(name);

CREATE TABLE IF NOT EXISTS entity_alias (
  entity_id   TEXT NOT NULL REFERENCES entity(entity_id) ON DELETE CASCADE,
  alias       TEXT NOT NULL,
  created_at  INTEGER NOT NULL,
  PRIMARY KEY (entity_id, alias)
);

CREATE INDEX IF NOT EXISTS idx_entity_alias_alias ON entity_alias(alias);

CREATE TABLE IF NOT EXISTS graph_edge (
  from_id     TEXT NOT NULL REFERENCES entity(entity_id) ON DELETE CASCADE,
  to_id       TEXT NOT NULL REFERENCES entity(entity_id) ON DELETE CASCADE,
  kind        TEXT NOT NULL,
  weight      REAL NOT NULL DEFAULT 1.0,
  created_at  INTEGER NOT NULL,
  updated_at  INTEGER NOT NULL,
  PRIMARY KEY (from_id, to_id, kind)
);

CREATE INDEX IF NOT EXISTS idx_graph_edge_to ON graph_edge(to_id);

-- ---------------------------------------------------------------------------
-- Scholarly works (WorkId: "doi:...", "arxiv:...", "pmid:...")
-- ---------------------------------------------------------------------------
-- `work_id` is the canonical normalized id form; `id_kind` is doi|arxiv|pmid
-- and `identifier` the normalized bare value. A work may be linked to the
-- resource it was observed in.
CREATE TABLE IF NOT EXISTS work (
  work_id      TEXT PRIMARY KEY,
  id_kind      TEXT NOT NULL,
  identifier   TEXT NOT NULL,
  title        TEXT,
  published_at INTEGER,
  modified_at  INTEGER,
  resource_id  TEXT REFERENCES resource(resource_id),
  created_at   INTEGER NOT NULL,
  updated_at   INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_work_resource ON work(resource_id);

-- ---------------------------------------------------------------------------
-- Resource freshness columns (kept distinct from first_seen / last_fetch)
-- ---------------------------------------------------------------------------
-- * published_at   — when the page/entry says it was published (unix seconds)
-- * modified_at    — when it says it was last modified
-- * fetched_at     — when *we* last fetched it (distinct from last_fetch, which
--                    is the frontier/fetch-observation projection; this is the
--                    freshness view used by retrieval ranking)
-- * verified_at    — when we last re-validated it (e.g. after a 304)
-- * feed_watermark — INTEGER high-water mark for a feed-discovered resource.
--                    Chosen as INTEGER (unix seconds of the newest entry seen)
--                    rather than TEXT: feed watermarks are publish timestamps,
--                    so an integer is monotone and range-scannable. Opaque
--                    per-feed cursor tokens (ETag/GUID) live in
--                    `feed.watermark`, which is TEXT — the two are kept apart.
ALTER TABLE resource ADD COLUMN published_at   INTEGER;
ALTER TABLE resource ADD COLUMN modified_at    INTEGER;
ALTER TABLE resource ADD COLUMN fetched_at     INTEGER;
ALTER TABLE resource ADD COLUMN verified_at    INTEGER;
ALTER TABLE resource ADD COLUMN feed_watermark INTEGER;
