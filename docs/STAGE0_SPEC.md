# Stage-0 Technical Specification

Status: frozen contract for the Stage-0 implementation. This document is the
authoritative interface definition for the first build milestone. Code must
conform to it; if code and spec disagree, the spec wins until it is revised in a
commit that says so.

Scope: a single-machine, single-writer foundation that proves the whole spine —
seed URL -> robots -> frontier lease -> HTTP fetch -> normalize -> ContentID ->
DocumentVersion -> ingest journal -> Tantivy commit -> manifest publish ->
`/search` — survives process restart, and rejects duplicate re-ingest.

Non-goals for Stage 0 (explicitly deferred, but interfaces left ready):

- dense/sparse retrieval, reranking, MMR, RRF fusion (Stage 2)
- agent/SearchState traversal (Stage 4)
- PDF/browser/OCR workers (Stage 1B)
- dataset builders: Common Crawl, OpenAlex, Wikidata (Stage 6)
- distributed crawl / service split (Stage 10)
- a dedicated vector database (deferred until Stage-5 benchmarks prove need)

---

## 1. Repository layout (Stage-0 subset)

```text
projects/search/
├── Cargo.toml                 # workspace root
├── crates/
│   ├── core/                  # IDs, config, errors, time, IndexManifest, limits
│   ├── metadata/              # SQLite single-writer subsystem
│   ├── ingest/                # journal + state machine + projection coordination
│   ├── frontier/              # per-origin queues, leases, backpressure
│   ├── fetcher/               # HTTP, URL canon, robots, CrawlPolicy, SSRF guard
│   ├── parser/                # response normalization (HTML -> NormalizedDoc)
│   ├── indexer/               # Tantivy base + overlay, manifest publish
│   ├── api/                   # POST /search, GET /health
│   └── agent0/                # Stage-0 driver binary (crawl + serve)
├── docs/STAGE0_SPEC.md        # this file
└── data/                      # runtime state, server-only, NOT synced
    ├── meta/                  # SQLite (WAL) + migrations
    ├── tantivy/base/          # immutable-ish base index
    ├── tantivy/overlay/       # recent-document overlay
    ├── blobs/<hash[0:2]>/     # Tier-3 raw cache
    └── text/<hash[0:2]>/      # Tier-2 extracted content
```

`data/` MUST live outside any Syncthing-synced path, and must appear in
`.stignore`. A WAL SQLite database or a Tantivy MmapDirectory copied mid-write
is corrupt, and WAL does not work over a network filesystem. Both are already
handled by `.stignore` + a gitignored `data/`.

Migrations are numbered files under `crates/metadata/migrations/` applied in
order by `crates/metadata/src/migrations.rs`; every migration must be idempotent
(`IF NOT EXISTS`) so a replay is safe. All tables the system needs — including
`published_watermark` — must be created by a migration, never lazily at runtime,
so a fresh database created from migrations alone is complete.

Frontier lease acquisition is a deliberate single-connection exception to the
batched metadata writer: acquiring a lease is an atomic read-modify-write
(`SELECT` eligible rows then `UPDATE` them in one transaction) that a batched
mutation queue cannot express without either losing atomicity or serializing all
work behind one channel. The frontier therefore owns one connection with WAL and
`busy_timeout`; at distributed scale this is the layer that would be replaced.

## 2. Identity model

Five semantic IDs (`core::ids`), strongly typed newtypes over `String`:

| ID | Meaning | Owner |
| :--- | :--- | :--- |
| `DomainId` | hostname | metadata |
| `SourceId` | logical collection targeted by routing | metadata |
| `ResourceId` | fetchable address | metadata + frontier |
| `WorkId` | logical intellectual object | metadata |
| `ContentId` | deduplicated payload | metadata + indexer |

Operational records that hang off those IDs — NOT a sixth semantic identity:

- `IngestOp` — an append-only journal entry keyed by `op_key`, the idempotency
  key `(resource_id, fetch_observation, representation_version)`.
- `FetchObservation` — one fetch attempt: status, validators (ETag /
  Last-Modified), content hash, time. A fetch does NOT imply a new
  `DocumentVersion`. `200 + new body` creates a version; `304` does not.

```text
Resource
  ├── FetchObservation #1  200 ETag A  -> DocumentVersion 1 -> ContentId X
  ├── FetchObservation #2  304         -> (same version)
  └── FetchObservation #3  200 ETag B  -> DocumentVersion 2 -> ContentId Y
```

This lets freshness learn "checked 20 times, changed twice" rather than only
version-creation times.

## 3. Ingestion state machine and journal

Lifecycle, strictly ordered:

```text
FETCHED -> NORMALIZED -> BLOB_WRITTEN -> METADATA_COMMITTED
        -> LEXICAL_INDEXED -> GRAPH_INDEXED -> VISIBLE
```

Rules:

1. The **journal is the source of truth**. Every transition appends one row to
   `ingest_event` (append-only, never updated or deleted). `ingest_op` holds the
   current high-water stage per `op_key` for fast lookup and is derivable from
   the journal.
2. Every projection is **idempotent**: applying `(op_key, stage)` twice is a
   no-op. Replay after a crash must converge to the same state.
3. **Search only exposes versions below the published watermark.** A document is
   returned by `/search` only when its `ingest_op.state = VISIBLE` AND its
   `doc_version` is covered by a published manifest generation.
4. **Rebuildability**: Tantivy and the graph can be rebuilt from
   `document_version` + blobs + journal. Nothing that only lives in Tantivy is
   authoritative.
5. A broken parser MUST NOT replace a good indexed version with empty text: if
   extraction yields no body and a prior good version exists, record
   `parse_failed` and leave the prior version visible.

## 4. SQLite schema (single-writer subsystem)

One process owns the writer. All crawler tasks emit mutations over a bounded
channel to a `MetadataWriter` that batches transactions. SQLite permits only one
WAL writer at a time; 64 tasks must not contend directly.

Connection pragmas: `journal_mode=WAL`, `synchronous=NORMAL`,
`busy_timeout=5000`, `wal_autocheckpoint=1000`, `foreign_keys=ON`.

DDL (authoritative; migrations live in `crates/metadata/migrations/`):

```sql
CREATE TABLE domain (
  domain_id  TEXT PRIMARY KEY,
  host       TEXT NOT NULL,
  created_at INTEGER NOT NULL
);

CREATE TABLE source (
  source_id  TEXT PRIMARY KEY,
  domain_id  TEXT NOT NULL REFERENCES domain(domain_id),
  scope_path TEXT NOT NULL DEFAULT '/',
  kind       TEXT NOT NULL DEFAULT 'web',
  created_at INTEGER NOT NULL
);

CREATE TABLE resource (
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

CREATE TABLE fetch_observation (
  obs_id        INTEGER PRIMARY KEY AUTOINCREMENT,
  resource_id   TEXT NOT NULL REFERENCES resource(resource_id),
  http_status   INTEGER NOT NULL,
  etag          TEXT,
  last_modified TEXT,
  content_hash  TEXT,
  fetched_at    INTEGER NOT NULL
);

CREATE TABLE content (
  content_id TEXT PRIMARY KEY,     -- hash of exact bytes
  exact_hash TEXT NOT NULL,
  text_path  TEXT,
  byte_len   INTEGER NOT NULL,
  created_at INTEGER NOT NULL
);

CREATE TABLE document_version (
  docver_id      TEXT PRIMARY KEY,  -- resource_id + ':' + version_no
  resource_id    TEXT NOT NULL REFERENCES resource(resource_id),
  version_no     INTEGER NOT NULL,
  content_id     TEXT REFERENCES content(content_id),
  title          TEXT,
  body_hash      TEXT,
  parser_version TEXT NOT NULL,
  published_at   INTEGER,           -- NULL = unknown, NEVER crawl time
  modified_at    INTEGER,
  fetched_at     INTEGER NOT NULL,
  created_at     INTEGER NOT NULL,
  UNIQUE(resource_id, version_no)
);

CREATE TABLE ingest_op (
  op_key      TEXT PRIMARY KEY,
  resource_id TEXT NOT NULL,
  docver_id   TEXT,
  state       TEXT NOT NULL,
  created_at  INTEGER NOT NULL,
  updated_at  INTEGER NOT NULL
);

CREATE TABLE ingest_event (            -- append-only journal
  seq         INTEGER PRIMARY KEY AUTOINCREMENT,
  op_key      TEXT NOT NULL,
  resource_id TEXT NOT NULL,
  stage       TEXT NOT NULL,
  detail      TEXT,
  at          INTEGER NOT NULL
);

CREATE TABLE frontier_entry (
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

CREATE TABLE origin_state (
  domain_id        TEXT PRIMARY KEY,
  concurrency      INTEGER NOT NULL DEFAULT 2,
  next_allowed_at  INTEGER NOT NULL DEFAULT 0,
  error_rate       REAL NOT NULL DEFAULT 0,
  adaptive_delay   INTEGER NOT NULL DEFAULT 0,
  active_leases    INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE robots_cache (
  domain_id  TEXT PRIMARY KEY,
  blob       TEXT,
  status     TEXT NOT NULL,          -- OK | UNAVAILABLE | UNREACHABLE | ERROR
  fetched_at INTEGER NOT NULL
);

CREATE TABLE crawl_policy (          -- local policy, distinct from robots
  source_id        TEXT PRIMARY KEY,
  max_rate         INTEGER NOT NULL DEFAULT 1,   -- req/sec
  allowed_types    TEXT NOT NULL DEFAULT 'text/html',
  auth_boundary    TEXT,
  browser_allowed  INTEGER NOT NULL DEFAULT 0,
  index_allowed    INTEGER NOT NULL DEFAULT 1,
  redistribution   TEXT NOT NULL DEFAULT 'metadata_only'
);

CREATE TABLE manifest (
  manifest_id  TEXT PRIMARY KEY,
  generation   INTEGER NOT NULL UNIQUE,
  published    INTEGER NOT NULL DEFAULT 0,
  doc_count    INTEGER NOT NULL,
  base_segments INTEGER NOT NULL,
  created_at   INTEGER NOT NULL,
  published_at INTEGER
);
```

Delete semantics never erase history: a removed resource sets `resource.status`
to `DELETED`, sets a tombstone, and moves `ACTIVE -> STALE -> DELETED ->
ARCHIVED`; `document_version` rows are retained.

## 5. Frontier lease model

`FETCHING` status alone is insufficient. Every dispatched job carries:

- `lease_until` — wall-clock deadline; expiry makes the job eligible again.
- `attempt` — incremented per dispatch.
- `owner` — worker id.
- `epoch` — **fencing token**, incremented on every re-dispatch.

Rules:

1. Dispatch is atomic: a job may be leased only if `status` is retryable AND
   (`lease_until` IS NULL OR `lease_until < now`). The dispatch increments
   `epoch`.
2. **Writes from an expired lease are rejected.** A worker commits results
   presenting the `epoch` it was dispatched with; the writer accepts only if the
   stored `epoch` still matches. This fences zombies: an expired-but-alive worker
   cannot double-write.
3. Retry belongs in the frontier with exponential backoff + jitter; workers never
   sleep. `429` obeys `Retry-After`. `404/410` mark the resource deleted.
4. Priority tiers: `0 QUERY_CRITICAL, 1 HOT, 2 NORMAL, 3 DISCOVERY, 4 BACKFILL`.
   Global queue dispatches through per-origin queues, never FIFO.
5. **Backpressure**: every channel is bounded — fetch->parse, parse->index,
   index->graph. If a downstream stage is slower, upstream must slow down, not
   buffer unboundedly. A full channel makes the dispatcher idle rather than drop.

## 6. Tantivy schema and base/overlay

Tantivy fields have a strict 1:1 relationship between ingested field and indexed
field — no field duplication or cross-field concatenation. We therefore send
explicit duplicate fields rather than relying on the engine.

Two indices searched together:

- `data/tantivy/base` — immutable-ish; new/live documents go to overlay first.
- `data/tantivy/overlay` — recent documents, small, searched immediately.

Fold: a background task periodically moves `VISIBLE` overlay docs whose
`visible_generation` is below the current generation into base, deleting them
from overlay (Tantivy deletes are term-based on `doc_id`).

Tantivy has no primary key: `doc_id` is our delete term. On update, delete the
`doc_id` term in base and write the new version to overlay.

Fields:

| field | type | options |
| :--- | :--- | :--- |
| `doc_id` | text (raw) | indexed, stored — delete key |
| `version` | u64 | fast |
| `resource_id` | text (raw) | indexed, stored, fast |
| `work_id` | text (raw) | indexed, stored |
| `content_id` | text (raw) | indexed, stored |
| `url` | text | indexed, stored |
| `title` | text | indexed, stored |
| `title_boost` | text | indexed (duplicate of title, boosted at query build) |
| `headings` | text | indexed |
| `body` | text | indexed, stored |
| `body_boost` | text | indexed (concatenation of title+headings+body) |
| `entities` | text | indexed, stored |
| `source_id` | text (raw) | indexed, stored, fast |
| `doc_type` | text (raw) | indexed, fast |
| `language` | text (raw) | indexed, fast |
| `published_at` | i64 | fast, stored, -1 when unknown |
| `modified_at` | i64 | fast, stored, -1 when unknown |
| `fetched_at` | i64 | fast, stored |
| `visible_generation` | u64 | fast |
| `tombstone` | u64 | fast (1 = deleted) |

Ordering: era is a separate rule, so temporal filtering uses `published_at` but
never derives it from `fetched_at`.

## 7. `/search` contract

```rust
struct SearchRequest {
    query: String,
    limit: usize,          // default 10
    offset: usize,         // default 0
    timezone: String,      // IANA, default UTC
    deadline_ms: Option<u64>,
    filters: Option<SearchFilters>,  // doc_type, source_id, language, time interval
}

struct QueryAnalysis {
    original: String,
    normalized_text: String,
    language: Option<String>,
    exact_phrases: Vec<String>,
    entities: Vec<String>,
    entity_hypotheses: Vec<String>,   // Stage 0: usually empty
    concepts: Vec<String>,
    hard_predicates: Vec<String>,
    soft_preferences: Vec<String>,
    time_interval: Option<TimeInterval>,
    preferred_types: Vec<String>,
    intent: Intent,                   // Factual | Navigational | Current | Navigational
    complexity: u8,                   // 0..=4 heuristic
}

struct SearchResponse {
    query_analysis: QueryAnalysis,
    results: Vec<SearchResult>,
    timing_ms: u64,
    // reproducibility: a response is reproducible from this tuple
    index_manifest_id: String,
    overlay_watermark: u64,
    graph_version: String,
    ranking_policy_version: String,
    query_plan_version: String,
    snapshot_id: String,              // hash of the above tuple
    complete: bool,                   // procedure completed, not "web searched"
    degradations: Vec<String>,        // lane/deadline failures -> partial response
}

struct SearchResult {
    doc_id: String,
    version: u64,
    resource_id: String,
    work_id: Option<String>,
    content_id: Option<String>,
    canonical_url: String,
    title: String,
    snippet: String,
    source_id: Option<String>,
    published_at: Option<i64>,
    modified_at: Option<i64>,
    fetched_at: i64,
    lane: String,                     // "lexical_base" | "lexical_overlay"
    provenance: Provenance,           // Snapshot | Overlay
    score: f32,
    signals: Vec<Signal>,             // per-signal provenance for "why this result?"
}
```

Every response returns at least one `Signal` so the UI's "Why this result?"
breakdown works from Stage 0.

## 8. Safety bounds (Stage-0 acceptance criteria, not Stage-5)

Enforced in `crates/core/limits.rs` and tested in Stage 0:

| bound | default |
| :--- | :--- |
| `max_response_bytes` | 5 MiB |
| `max_decompressed_bytes` | 25 MiB |
| `max_redirects` | 5 |
| `max_parse_ms` | 2000 |
| `max_archive_expansion` | 100 MiB |
| `max_archive_depth` | 3 |
| `max_pdf_pages` | 500 |
| `max_browser_ms` | 15000 |

Never trust remote `Content-Length`; enforce while streaming.

SSRF: every URL and every redirect target is validated. Block `127/8`, RFC1918,
link-local, `169.254.0.0/16` (cloud metadata), unique-local IPv6, and non-HTTP
schemes. Resolve DNS and re-check the resolved IP (DNS rebinding), not just the
literal host.

## 9. Robots and CrawlPolicy (distinct subsystems)

`RobotsPolicy` answers only: may a crawler request this URL under REP. RFC 9309
semantics, tested explicitly:

1. A successfully fetched file's rules MUST be followed.
2. **Unavailable** (fetch yields 4xx): crawler MAY access any resource.
3. **Unreachable** (5xx or network error): crawler MUST assume complete
   disallow.
4. Parsing errors: MUST parse each line and use the parseable rules.
5. Caching: SHOULD NOT use a cached copy > 24 hours unless the file is
   unreachable.
6. Parsing limit MUST be at least 500 KiB.

`CrawlPolicy` is local policy and MUST NOT be folded into robots:

- source-specific restrictions, auth/login boundaries
- accepted content types
- maximum rate (per source)
- redistribution / index policy
- whether browser rendering is permitted

## 10. Acceptance tests (Stage-0 exit gate)

The single most important test renders this sequence and then breaks it:

```text
seed URL
  -> robots evaluated
  -> frontier lease issued
  -> HTTP fetch succeeds
  -> content normalized
  -> ContentId assigned
  -> DocumentVersion committed
  -> ingest journal records each projection
  -> Tantivy commit
  -> manifest published
  -> /search finds the document
  -> process restarts
  -> same search still works
  -> re-ingesting the same URL/content creates no duplicate
```

Fault suite (each is a real test, not a comment):

1. kill the process mid-index-commit, restart, verify no corruption and replay.
2. disk-full on blob write -> clean failure, no partial `document_version`.
3. corrupt a blob -> hash mismatch detected, version not published.
4. `304` after an older `200` -> no new `DocumentVersion`, freshness updated.
5. `429` with `Retry-After` -> job rescheduled, not dropped.
6. redirect from public URL to `127.0.0.1` -> blocked by SSRF guard.
7. **zombie worker**: lease expired while worker alive -> its write rejected by
   epoch/fencing mismatch.
8. at-least-once replay: deliver the same `IngestOp` twice -> no duplicate doc.
9. delete a document while an old searcher is active -> old searcher still works
   (immutable snapshot).

## 11. Observability

`tracing` for structured logs plus Prometheus-compatible counters. OTLP is an
optional export only; the core does not depend on vendor SDK abstractions.
Every search logs: query class, plan, candidate provenance, latency, gaps,
freshness outcome, model/index versions.
