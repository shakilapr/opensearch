# Stage 6 Spec — Dataset builder

Frozen interfaces. New crate `crates/dataset` (package `search-dataset`) + `datasets/` output
dir. Adapters must be **testable offline** with committed fixtures — no network in tests.

## Frozen interfaces

```rust
pub struct Record {
    pub id: String,
    pub source: String,          // "commoncrawl" | "wikidata" | "openalex" | "oai-pmh" | ...
    pub url: Option<String>,
    pub title: Option<String>,
    pub text: String,
    pub published_at: Option<i64>,
    pub extras: std::collections::BTreeMap<String, String>,
}

pub struct Page { pub records: Vec<Record>, pub next: Option<String> }

pub trait DatasetAdapter {
    fn name(&self) -> &str;
    /// Fetch one page of records. `cursor` is the opaque token from a previous page.
    fn page(&mut self, cursor: Option<&str>) -> Result<Page, DatasetError>;
}

pub struct CommonCrawlAdapter;   // parses WAT/WET-ish JSONL (fixture)
pub struct WikidataAdapter;      // parses entity JSON (fixture)
pub struct OpenAlexAdapter;      // parses OpenAlex work JSON (fixture)
pub struct OaiPmhAdapter;        // parses OAI-PMH XML incl. resumptionToken

pub struct PackManifest {
    pub name: String, pub version: String, pub records: u64,
    pub shards: Vec<ShardInfo>, pub created_at: i64, pub schema: u32,
}
pub struct ShardInfo { pub path: String, pub records: u64, pub checksum: String }

/// Write records as JSONL shards of at most `shard_size` records each, plus a manifest.
pub fn write_pack(dir: &std::path::Path, name: &str, version: &str,
                  records: impl IntoIterator<Item = Record>, shard_size: usize) -> Result<PackManifest, DatasetError>;

/// Re-read + verify a pack on disk (shard checksums + record counts match the manifest).
pub fn validate_pack(dir: &std::path::Path) -> Result<PackManifest, DatasetError>;

/// End-to-end: pull from an adapter until exhausted (bounded by `max_records`), write a pack.
pub fn build_from_adapter(adapter: &mut dyn DatasetAdapter, dir: &std::path::Path,
                          name: &str, version: &str, shard_size: usize, max_records: u64)
                          -> Result<PackManifest, DatasetError>;
```

`DatasetError { Io, Parse(String), Http(String), Exhausted }` via thiserror.

## Requirements

- Each adapter parses its **committed fixture** under `datasets/fixtures/<source>/` and
  supports cursor-based paging. OAI-PMH must follow `resumptionToken`.
- `write_pack` + `validate_pack` round-trip: a tampered shard fails validation.
- Adapters that would need live network take an optional base URL / file path injected by the
  caller; never hardcode a key or credential.

## Exit gate

A committed fixture from each source is parsed into `Record`s, a pack is written, validated,
and a tampered shard is rejected — all hermetic.
