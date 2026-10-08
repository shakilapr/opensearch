# Stage 5 Spec — Scale

Frozen interfaces. New crates + one scheduler addition + migration 0004. Do not redesign.

## New crates

| crate dir          | package            | owns                                        |
|--------------------|--------------------|---------------------------------------------|
| `crates/dedup`     | `search-dedup`     | SimHash64, MinHash+LSH banding, shingling  |
| `crates/tiers`     | `search-tiers`     | HOT/WARM/COLD *storage* tiers + move planner|
| `crates/patterns`  | `search-patterns`  | URL templatization + pattern learning       |

## Frozen interfaces

```rust
// dedup
pub fn shingles(text: &str, k: usize) -> BTreeSet<String>;
pub struct SimHash64(pub u64);
impl SimHash64 { pub fn of_tokens<'a>(tokens: impl IntoIterator<Item = &'a str>) -> Self;
                 pub fn hamming(&self, other: &SimHash64) -> u32; }
pub struct NearDupIndex { /* bands x rows */ }
impl NearDupIndex {
    pub fn new(bands: usize, rows: usize, max_hamming: u32) -> Self;
    pub fn insert(&mut self, id: impl Into<String>, fp: SimHash64);
    pub fn candidates(&self, fp: &SimHash64) -> Vec<String>;
    pub fn is_near_dup(&self, fp: &SimHash64) -> bool;
}
pub struct MinHash { /* k permutations */ }
impl MinHash { pub fn new(k: usize) -> Self; pub fn signature(&self, shingles: &BTreeSet<String>) -> Vec<u64>; }
pub fn lsh_bands(sig: &[u64], bands: usize) -> Vec<u64>;

// tiers (storage placement — distinct from crawler freshness classes)
pub enum Tier { Hot, Warm, Cold }
pub struct TierPolicy { pub hot_max_age_secs: i64, pub warm_max_age_secs: i64, pub hot_max_bytes: u64 }
pub struct DocMeta { pub doc_id: String, pub last_access: i64, pub last_fetch: i64, pub bytes: u64 }
pub fn classify(meta: &DocMeta, policy: &TierPolicy, now: i64) -> Tier;
pub enum TierMove { Promote(String, Tier), Demote(String, Tier) }
pub fn plan(current: &[(DocMeta, Tier)], policy: &TierPolicy, now: i64) -> Vec<TierMove>;

// patterns
pub fn templatize(url: &str) -> String;   // digits/UUID/date runs -> tokens; keeps host+path shape
pub struct UrlPattern { pub template: String, pub support: u64 }
pub struct PatternLearner { /* counts */ }
impl PatternLearner {
    pub fn new() -> Self;
    pub fn observe(&mut self, url: &str);
    pub fn patterns(&self) -> Vec<UrlPattern>;          // support-desc
    pub fn is_paginated(&self, url: &str) -> bool;      // matches a high-support page-number template
}
```

## scheduler addition (existing crate)

```rust
pub struct ChangeHistory { pub checks: u32, pub changes: u32 }
impl ChangeHistory { pub fn observed_rate(&self) -> f64; }   // changes/checks, 0.0 when no checks
/// Blend observed rate with the prior; never below `min_delay` or above `max_delay`.
pub fn next_delay_secs(history: &ChangeHistory, prior_rate: f64, class: SourceClass,
                       min_delay: i64, max_delay: i64) -> i64;
```

## metadata migration 0004

Add `storage_tier` TEXT (default `'hot'`) to `resource`, plus an index. Bump `SCHEMA_VERSION`
to 4. Add `Mutation::UpdateStorageTier { resource_id, tier }` and a reader
`resource_tier(resource_id)`. Follow the existing migration-runner pattern exactly.

## Exit gate

Near-duplicate detection groups a set of ≥3 reworded documents as one cluster while keeping a
genuinely distinct document out; tier planning demotes cold, rarely-accessed docs and keeps
hot ones; `templatize` collapses paginated URLs (`/page/3`, `/page/4`) to one template.
All hermetic; no network.
