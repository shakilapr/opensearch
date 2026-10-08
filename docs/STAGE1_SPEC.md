# Stage 1 Spec — 10K LLM Search MVP

Frozen interfaces for Stage 1. Stage 0 (see `STAGE0_SPEC.md`) is complete and green.
Do not change Stage-0 contracts; add alongside them.

## Goal

Search complicated LLM / tech questions against the **local** index with no external search
API, and have the UI show per-signal provenance ("Why this result?").

## New crates (each owns its own files; do not edit other crates)

| crate dir            | package          | owns                                            |
|----------------------|------------------|-------------------------------------------------|
| `crates/discovery`   | `search-discovery` | sitemap + RSS/Atom feed discovery             |
| `crates/router`      | `search-router`  | MIME-based response router                      |
| `crates/browser`     | `search-browser` | browser-escalation interface + hermetic mock    |
| `crates/pdf`         | `search-pdf`     | PDF pipeline (size→hash→text-layer→classify)     |
| `crates/sources`     | `search-sources` | `SourceProfile` registry (pure types + logic)   |
| `crates/graph`       | `search-graph`   | entity aliases, concept/source graph, WorkId    |
| `crates/retrieval`   | `search-retrieval` | source-first retrieval over source graph + BM25 |

Dependency rule: these crates depend on `search-core` (and, for `retrieval`, on
`search-indexer` + `search-graph`). `search-core` never depends on them. `search-metadata`
stays free of them too: it owns persistence with its **own plain row structs**.

## Frozen interfaces

### discovery (`search-discovery`)

```rust
pub struct DiscoveredResource { pub url: String, pub lastmod: Option<String>, pub changefreq: Option<String>,
                                pub priority: Option<f32>, pub kind: ResourceKind, pub title: Option<String>,
                                pub published_at: Option<i64>, pub source_url: String }
pub enum ResourceKind { Page, FeedEntry, Sitemap, Image, Other }

/// Parse a sitemap.xml (urlset) or sitemap index; handles <sitemapindex> and .gz.
pub fn parse_sitemap(xml: &[u8], source_url: &str) -> Result<Vec<DiscoveredResource>, DiscoverError>;
/// Parse RSS 2.0 or Atom 1.0.
pub fn parse_feed(xml: &[u8], source_url: &str) -> Result<Vec<DiscoveredResource>, DiscoverError>;
/// Sitemap URLs advertised in a robots.txt body.
pub fn sitemaps_in_robots(robots_txt: &str) -> Vec<String>;
/// Child-sitemap URLs from a sitemap index.
pub fn sitemap_index_urls(xml: &[u8]) -> Result<Vec<String>, DiscoverError>;
```

Bounds: reject bodies > 32 MiB decompressed; cap returned resources at 50_000.

### router (`search-router`)

```rust
pub enum Route { Html, Pdf, Feed, Json, Xml, PlainText, Image, Archive, Other(String) }
pub struct Routing { pub route: Route, pub charset: Option<String> }
pub fn classify(content_type: Option<&str>, url: &str, sniff: &[u8]) -> Routing;
```

Rules: prefer `Content-Type`; sniff magic bytes when absent/`octet-stream`
(`%PDF-`, `<?xml`+`<rss`/`<feed`, `{`/`[`, `PK\x03\x04`, image magic). URL extension is the
last resort. Never trust the declared type over an unambiguous magic number.

### browser (`search-browser`)

```rust
#[async_trait::async_trait]
pub trait BrowserFetcher: Send + Sync {
    async fn fetch(&self, url: &str, limits: BrowserLimits) -> Result<BrowserResult, BrowserError>;
}
pub struct BrowserLimits { pub max_ms: u64, pub max_dom_bytes: usize, pub block_media: bool, pub max_redirects: u8 }
pub struct BrowserResult { pub final_url: String, pub html: String, pub status: u16, pub json_endpoints: Vec<String>, pub elapsed_ms: u64 }
```

- `MockBrowser` (default, hermetic) serves registered fixtures; used by tests.
- `json_endpoints`: URLs of XHR/fetch calls observed via network interception, deduped.
- Real Chromium impl lives behind `feature = "chromium"` (must not be required to build).

### pdf (`search-pdf`)

```rust
pub struct PdfOutcome { pub text: String, pub page_count: u32, pub used_ocr: bool, pub classifier: PdfClass }
pub enum PdfClass { TextLayer, ScannedImage, Scholarly, Unknown }
pub struct PdfLimits { pub max_bytes: u64, pub max_pages: u32, pub max_ms: u64 }
pub fn extract(bytes: &[u8], limits: PdfLimits) -> Result<PdfOutcome, PdfError>;
/// Decide text-layer vs scanned; scholarly heuristic (DOI/arXiv/abstract markers).
pub fn classify(text: &str, page_count: u32) -> PdfClass;
/// Queue an OCR job when no text layer exists (interface only in Stage 1).
pub enum OcrJob { Queued { page_count: u32 }, Skipped { reason: String } }
```

### sources (`search-sources`)

```rust
pub struct SourceProfile { pub source_id: String, pub domain: String, pub topics: Vec<String>,
                           pub entities: Vec<String>, pub url_patterns: Vec<String>, pub authority: f32,
                           pub change_rate: f32, pub js_probability: f32, pub fetch_count: u64, pub error_count: u64 }
impl SourceProfile {
    pub fn new(source_id: impl Into<String>, domain: impl Into<String>) -> Self;
    pub fn observe_fetch(&mut self, ok: bool);          // updates fetch/error stats + change_rate
    pub fn matches_url(&self, url: &str) -> bool;        // first-matching glob-ish pattern
    pub fn authority_prior(&self) -> f32;                // smoothed, in (0,1)
}
```

### graph (`search-graph`)

```rust
pub struct Entity { pub entity_id: String, pub name: String, pub kind: EntityKind, pub aliases: Vec<String> }
pub enum EntityKind { Person, Organization, Product, Concept, Place, Work, Other(String) }
pub struct Edge { pub from: String, pub to: String, pub kind: EdgeKind, pub weight: f32 }
pub enum EdgeKind { AliasOf, RelatedTo, Cites, AuthoredBy, PublishedBy }
pub struct Graph { /* indexes in memory */ }
impl Graph {
    pub fn new() -> Self;
    pub fn upsert_entity(&mut self, e: Entity);
    pub fn add_edge(&mut self, e: Edge);
    pub fn resolve_name(&self, name: &str) -> Vec<&Entity>;   // exact or alias, case-insensitive
    pub fn expand(&self, entity_id: &str, max_hops: u8) -> Vec<&Entity>;
}
/// Work identity from a scholarly identifier. Returns canonical WorkId + the id kind.
pub enum WorkIdKind { Doi, Arxiv, Pmid }
pub fn work_id_from_identifier(kind: WorkIdKind, value: &str) -> Option<String>;
pub fn detect_identifier(text: &str) -> Option<(WorkIdKind, String)>;  // scans for DOI/arXiv/PMID
```

`work_id_from_identifier` normalizes: DOI → lowercase, strip `https://doi.org/` / `doi:`;
arXiv → strip `arXiv:` and old-style `math.GT/0309136` kept verbatim; PMID → digits only.
WorkId string form: `"doi:10.1234/x"`, `"arxiv:2301.00001"`, `"pmid:12345678"`.

### retrieval (`search-retrieval`)

```rust
pub struct Candidate { pub doc_id: String, pub score: f32, pub lane: Lane, pub source_id: Option<String>, pub title: Option<String>, pub url: Option<String> }
pub enum Lane { Bm25, SourceFirst }
/// Source-first retrieval: for each source whose profile matches the query, run BM25 scoped
/// to that source, then merge BM25-global + source-scoped candidates.
pub trait LaneSearcher: Send + Sync { fn search(&self, query: &str, limit: usize, source_id: Option<&str>) -> Result<Vec<Candidate>, RetrievalError>; }
pub struct SourceFirst { pub graph: /* ref */ }
pub fn merge_dedup(cands: Vec<Candidate>) -> Vec<Candidate>;  // by doc_id, max score, stable order
```

## Metadata additions (Stage 1)

Migration `0003_stage1.sql` + repositories with **plain row structs** (no dep on the new
crates). Tables: `sitemap`, `feed`, `source_profile`, `entity`, `entity_alias`,
`graph_edge`, `work`. Freshness columns on `resource`: `published_at`, `modified_at`,
`fetched_at`, `verified_at`, `feed_watermark` (keep distinct). All writes go through the
existing single-writer `Mutation` API; no runtime DDL.

## Fetcher additions (Stage 1)

Conditional refresh: store `etag`/`last_modified` per resource; send `If-None-Match` /
`If-Modified-Since`; treat `304` as `Unchanged` **without** re-parsing or re-indexing.

## API + UI (Stage 1)

- `search-api`: populate `SearchResult.signals` (per-signal provenance: bm25, source_authority,
  freshness, entity_match, graph_expansion) so "Why this result?" has real data; expose
  `live_overlay` flag when a result came from the recent overlay window.
- `ui/src/services/hexxicaClient.ts` → real HTTP client against `POST /search`; keep the
  existing component tree and `SearchResponse` shape; `Signal` breakdown rendered in the
  existing result component. `npm run lint` and `npm test` must pass.

## Exit gate

A fixed set of LLM-related multi-word/ambiguous queries returns relevant results from the
local index with no external search API, and the UI shows per-signal provenance.
