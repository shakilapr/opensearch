-- Migration 0006: the identity layer above resources — websites, sections, sitemap
-- entries, work locations, and per-source concept profiles.
--
-- Migration 0005 added the semantic layer (concepts, weighted associations,
-- passages, links). This one adds the identities those associations hang from, so
-- the semantic index never has to store a URL and a URL never has to store a
-- meaning. See `docs/SEMANTIC_INDEX_ARCHITECTURE.md` §2–§3.
--
-- Two relationships are deliberately *not* collapsed:
--
--   * A **website** is not a hostname. One site can span several hostnames
--     (`example.com`, `www.example.com`, `docs.example.com`), and one hostname can
--     host several unrelated sites (a shared platform). `website_host` therefore
--     holds the many-to-many mapping and `domain` stays what it is: a DNS host.
--   * A **work** is not a location. A paper can live at a publisher URL, a preprint
--     URL and a repository URL; `work_location` records each copy with its role,
--     so the same work is never indexed as three unrelated documents.

PRAGMA foreign_keys = ON;

-- ---------------------------------------------------------------------------
-- Websites: the user-facing unit above a domain
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS website (
  web_id        TEXT PRIMARY KEY,
  name          TEXT,
  language      TEXT,
  source_type   TEXT,
  authority     REAL,
  created_at    INTEGER NOT NULL,
  updated_at    INTEGER NOT NULL
);

-- One website may serve several hostnames; one hostname may host several sites.
CREATE TABLE IF NOT EXISTS website_host (
  web_id  TEXT NOT NULL REFERENCES website(web_id) ON DELETE CASCADE,
  host    TEXT NOT NULL,
  primary_host INTEGER NOT NULL DEFAULT 0,
  created_at   INTEGER NOT NULL,
  PRIMARY KEY (web_id, host)
);

CREATE INDEX IF NOT EXISTS idx_website_host_host ON website_host(host);

-- ---------------------------------------------------------------------------
-- Logical sections inside a website (documentation trees, journal sections…)
-- ---------------------------------------------------------------------------
-- `path_prefix` is the URL prefix the section owns (`/docs/v2/`), which is what
-- makes a section machine-usable without crawling its whole tree.
CREATE TABLE IF NOT EXISTS source_section (
  section_id         TEXT PRIMARY KEY,
  web_id             TEXT REFERENCES website(web_id) ON DELETE CASCADE,
  parent_section_id  TEXT REFERENCES source_section(section_id) ON DELETE SET NULL,
  name               TEXT,
  path_prefix        TEXT,
  topics             TEXT NOT NULL DEFAULT '[]',
  created_at         INTEGER NOT NULL,
  updated_at         INTEGER NOT NULL,
  CHECK (section_id <> COALESCE(parent_section_id, ''))
);

CREATE INDEX IF NOT EXISTS idx_source_section_web ON source_section(web_id);
CREATE INDEX IF NOT EXISTS idx_source_section_parent ON source_section(parent_section_id);

-- ---------------------------------------------------------------------------
-- Sitemap entries: advertised URLs, distinct from verified pages
-- ---------------------------------------------------------------------------
-- A sitemap is a discovery hint, not a claim that the page exists or that its
-- content matches. `advertised_last_mod` is what the sitemap said, kept apart
-- from `resource.modified_at`, which is what the fetch observed.
CREATE TABLE IF NOT EXISTS sitemap_entry (
  sitemap_id           TEXT NOT NULL REFERENCES sitemap(sitemap_id) ON DELETE CASCADE,
  url                  TEXT NOT NULL,
  resource_id          TEXT,
  advertised_last_mod  TEXT,
  created_at           INTEGER NOT NULL,
  PRIMARY KEY (sitemap_id, url)
);

CREATE INDEX IF NOT EXISTS idx_sitemap_entry_resource ON sitemap_entry(resource_id);
CREATE INDEX IF NOT EXISTS idx_sitemap_entry_url ON sitemap_entry(url);

-- ---------------------------------------------------------------------------
-- Work locations: one work, several copies
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS work_location (
  work_id      TEXT NOT NULL REFERENCES work(work_id) ON DELETE CASCADE,
  resource_id  TEXT NOT NULL REFERENCES resource(resource_id) ON DELETE CASCADE,
  relationship TEXT NOT NULL DEFAULT 'copy',
  preferred    INTEGER NOT NULL DEFAULT 0,
  created_at   INTEGER NOT NULL,
  PRIMARY KEY (work_id, resource_id),
  CHECK (relationship IN ('publisher','preprint','repository','author_copy','archive','copy'))
);

CREATE INDEX IF NOT EXISTS idx_work_location_resource ON work_location(resource_id);

-- ---------------------------------------------------------------------------
-- Source-level concept profiles: what a whole site is about
-- ---------------------------------------------------------------------------
-- Aggregated from `doc_concept`, but normalised by document count so a site with
-- 200,000 pages cannot outrank a precise 20-page site merely by volume.
-- `doc_count` is stored so the normalisation is auditable after the fact.
CREATE TABLE IF NOT EXISTS source_concept (
  source_id     TEXT NOT NULL,
  concept_id    TEXT NOT NULL REFERENCES concept(concept_id) ON DELETE CASCADE,
  weight        REAL NOT NULL,
  doc_count     INTEGER NOT NULL DEFAULT 0,
  model_version TEXT,
  updated_at    INTEGER NOT NULL,
  PRIMARY KEY (source_id, concept_id),
  CHECK (weight >= 0.0 AND weight <= 1.0)
);

CREATE INDEX IF NOT EXISTS idx_source_concept_concept ON source_concept(concept_id, weight DESC);

-- ---------------------------------------------------------------------------
-- Attach the new identities to the existing resource row
-- ---------------------------------------------------------------------------
-- Both columns are nullable: every existing resource keeps working, and the
-- identity layer can be populated incrementally without a rewrite.
-- `ALTER TABLE ... ADD COLUMN` has no `IF NOT EXISTS`; replay safety comes from the
-- migration runner's ledger, which applies each version at most once.
ALTER TABLE resource ADD COLUMN web_id TEXT;
ALTER TABLE resource ADD COLUMN section_id TEXT;

CREATE INDEX IF NOT EXISTS idx_resource_web ON resource(web_id);
CREATE INDEX IF NOT EXISTS idx_resource_section ON resource(section_id);
