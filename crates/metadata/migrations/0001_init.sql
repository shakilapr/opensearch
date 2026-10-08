-- Migration 0001: initial schema (spec §4).
-- Authoritative DDL. Idempotent (IF NOT EXISTS) so replay is safe.

PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS domain (
  domain_id  TEXT PRIMARY KEY,
  host       TEXT NOT NULL,
  created_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS source (
  source_id  TEXT PRIMARY KEY,
  domain_id  TEXT NOT NULL REFERENCES domain(domain_id),
  scope_path TEXT NOT NULL DEFAULT '/',
  kind       TEXT NOT NULL DEFAULT 'web',
  created_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS resource (
  resource_id   TEXT PRIMARY KEY,
  url           TEXT NOT NULL,
  canonical_url TEXT NOT NULL,
  domain_id     TEXT NOT NULL REFERENCES domain(domain_id),
  source_id     TEXT,
  kind          TEXT NOT NULL DEFAULT 'html',
  status        TEXT NOT NULL DEFAULT 'NEW',
  first_seen    INTEGER NOT NULL,
  last_fetch    INTEGER
);

CREATE TABLE IF NOT EXISTS fetch_observation (
  obs_id        INTEGER PRIMARY KEY AUTOINCREMENT,
  resource_id   TEXT NOT NULL REFERENCES resource(resource_id),
  http_status   INTEGER NOT NULL,
  etag          TEXT,
  last_modified TEXT,
  content_hash  TEXT,
  fetched_at    INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS content (
  content_id TEXT PRIMARY KEY,
  exact_hash TEXT NOT NULL,
  text_path  TEXT,
  byte_len   INTEGER NOT NULL,
  created_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS document_version (
  docver_id      TEXT PRIMARY KEY,
  resource_id    TEXT NOT NULL REFERENCES resource(resource_id),
  version_no     INTEGER NOT NULL,
  content_id     TEXT REFERENCES content(content_id),
  title          TEXT,
  body_hash      TEXT,
  parser_version TEXT NOT NULL,
  published_at   INTEGER,
  modified_at    INTEGER,
  fetched_at     INTEGER NOT NULL,
  created_at     INTEGER NOT NULL,
  UNIQUE(resource_id, version_no)
);

CREATE TABLE IF NOT EXISTS ingest_op (
  op_key      TEXT PRIMARY KEY,
  resource_id TEXT NOT NULL,
  docver_id   TEXT,
  state       TEXT NOT NULL,
  created_at  INTEGER NOT NULL,
  updated_at  INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS ingest_event (
  seq         INTEGER PRIMARY KEY AUTOINCREMENT,
  op_key      TEXT NOT NULL,
  resource_id TEXT NOT NULL,
  stage       TEXT NOT NULL,
  detail      TEXT,
  at          INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS frontier_entry (
  resource_id       TEXT PRIMARY KEY,
  url               TEXT NOT NULL,
  domain_id         TEXT NOT NULL,
  priority          INTEGER NOT NULL DEFAULT 2,
  earliest_fetch_at INTEGER NOT NULL,
  retry_count       INTEGER NOT NULL DEFAULT 0,
  status            TEXT NOT NULL DEFAULT 'NEW',
  lease_until       INTEGER,
  attempt           INTEGER NOT NULL DEFAULT 0,
  owner             TEXT,
  epoch             INTEGER NOT NULL DEFAULT 0,
  expected_value    REAL,
  discovered_from   TEXT
);

CREATE TABLE IF NOT EXISTS origin_state (
  domain_id        TEXT PRIMARY KEY,
  concurrency      INTEGER NOT NULL DEFAULT 2,
  next_allowed_at  INTEGER NOT NULL DEFAULT 0,
  error_rate       REAL NOT NULL DEFAULT 0,
  adaptive_delay   INTEGER NOT NULL DEFAULT 0,
  active_leases    INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS robots_cache (
  domain_id  TEXT PRIMARY KEY,
  blob       TEXT,
  status     TEXT NOT NULL,
  fetched_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS crawl_policy (
  source_id        TEXT PRIMARY KEY,
  max_rate         INTEGER NOT NULL DEFAULT 1,
  allowed_types    TEXT NOT NULL DEFAULT 'text/html',
  auth_boundary    TEXT,
  browser_allowed  INTEGER NOT NULL DEFAULT 0,
  index_allowed    INTEGER NOT NULL DEFAULT 1,
  redistribution   TEXT NOT NULL DEFAULT 'metadata_only'
);

CREATE TABLE IF NOT EXISTS manifest (
  manifest_id   TEXT PRIMARY KEY,
  generation    INTEGER NOT NULL UNIQUE,
  published     INTEGER NOT NULL DEFAULT 0,
  doc_count     INTEGER NOT NULL,
  base_segments INTEGER NOT NULL,
  created_at    INTEGER NOT NULL,
  published_at  INTEGER
);

CREATE INDEX IF NOT EXISTS idx_resource_domain ON resource(domain_id);
CREATE INDEX IF NOT EXISTS idx_docver_resource ON document_version(resource_id);
CREATE INDEX IF NOT EXISTS idx_ingest_event_op ON ingest_event(op_key);
CREATE INDEX IF NOT EXISTS idx_frontier_dispatch ON frontier_entry(status, priority, earliest_fetch_at);
