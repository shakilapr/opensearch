# Stage 8 Spec — Evaluation, observability & hardening

Frozen interfaces. New crates `search-trace` and `search-cache`, a priors addition to
`search-sources`, and a workspace-level `search-acceptance` test crate.

## crates/trace

```rust
pub enum Phase { Query, Analyze, Route, Retrieve, Refresh, Rerank, Navigate, Answer }
pub struct TraceEvent { pub ts_ms: u64, pub query_id: String, pub phase: Phase,
                        pub action: String, pub detail: std::collections::BTreeMap<String,String>,
                        pub policy_tier: Option<String>, pub cost_ms: u64 }
pub trait TraceSink: Send + Sync { fn emit(&self, ev: &TraceEvent); }
pub struct NullSink;
pub struct JsonlTraceSink;            // new(path)
impl JsonlTraceSink { pub fn new(path: impl Into<std::path::PathBuf>) -> std::io::Result<Self>; }
pub struct PolicyTrace;               // new(Box<dyn TraceSink>), new_query_id(), emit(phase, action, cost_ms)
pub fn read_trace(path: &std::path::Path) -> std::io::Result<Vec<TraceEvent>>;
```

## crates/cache

```rust
pub struct Generation(pub u64);
pub struct CacheConfig { pub ttl_ms: u64, pub max_entries: usize }
pub struct Cache<V>;                  // LRU + TTL
impl<V: Clone> Cache<V> {
    pub fn new(config: CacheConfig) -> Self;
    pub fn get(&self, key: &str, now_ms: u64) -> Option<V>;         // None when absent/expired
    pub fn put(&self, key: impl Into<String>, value: V, now_ms: u64);
    pub fn invalidate_generation(&self, gen: Generation);           // drops entries stamped older
    pub fn len(&self) -> usize;
}
```

Interior mutability (`Mutex`) so a `&self` cache is shareable. `invalidate_generation` must be
observable: entries carry the `Generation` they were written under and are dropped when it
advances.

## crates/sources addition

```rust
pub struct SourcePrior { pub source_id: String, pub quality: f32, pub samples: u64 }
pub struct PriorTable;                // new(), observe(source_id, quality: f32), prior_for(source_id) -> f32,
                                      // priors() -> Vec<SourcePrior>
```
Blend toward a default prior when samples are few (Bayesian-ish; `samples`-weighted). Never NaN.

## crates/acceptance (workspace-level integration tests)

Twelve acceptance tests over the whole system, hermetic (no network; fixture servers), each
asserting ONE named end-to-end property. Cover: ingest→search round trip; restart safety;
duplicate rejection; robots denial; SSRF block; conditional 304; discovery without full fetch;
PDF routing; freshness columns; query-triggered refresh surfaces a new doc; per-origin fan-out;
MCP `search` tool round trip. Print a summary line per test.

## Exit gate

All twelve acceptance tests pass; a trace JSONL round-trips through `read_trace`; cache TTL and
generation invalidation are observable; `prior_for` is stable and bounded.
