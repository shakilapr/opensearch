# Stage 3 Spec — Live freshness loop

Frozen interfaces for Stage 3. Stage 2 crates (`eval`/`embed`/`rerank`) and the Stage-1
pipeline are in place. Do not redesign; extend.

## Goal

A "latest / today" query returns current results the local index did not yet contain,
without any commercial search engine, and refresh does not storm (per-origin limits hold).

## New crate

| crate dir            | package           | owns                                                     |
|----------------------|-------------------|----------------------------------------------------------|
| `crates/scheduler`   | `search-scheduler` | freshness classes, change model, priority tiers, budget ledger, activation scheduler, query-triggered refresh |

Depends on `search-core` only (pure logic). Wiring into the crawl loop happens in `agent0`.

## Frozen interfaces

```rust
/// HOT/WARM/COLD source classes (crawler §39).
pub enum SourceClass { Hot, Warm, Cold }
pub fn classify_source(change_rate: f32, authority: f32) -> SourceClass;

/// Learned change model: P(changed) = 1 - exp(-rate * age). rate is per-second, age in secs.
pub fn change_probability(rate: f64, age_secs: f64) -> f64;

/// Scheduler priority tiers (crawler §40, engine §12).
pub enum Priority { P0QueryCritical, P1Hot, P2Normal, P3Discovery, P4Backfill }
impl Priority { pub fn rank(&self) -> u8; }   // 0 = highest

/// Atomic budget ledger: charged BEFORE dispatch, never after (engine-arch §13).
pub struct BudgetLedger { /* atomic */ }
impl BudgetLedger {
    pub fn new(capacity: u64) -> Self;
    pub fn try_charge(&self, cost: u64) -> bool;   // false when it would exceed capacity
    pub fn refund(&self, cost: u64);               // on early abort
    pub fn spent(&self) -> u64;
    pub fn remaining(&self) -> u64;
}

/// A single unit of refresh work the scheduler decides to run.
pub struct Activation { pub url: String, pub priority: Priority, pub reason: ActivationReason,
                        pub origin: String, pub due_at: i64 }
pub enum ActivationReason { QueryCritical, Changed, Due, Discovery, Backfill }

pub struct SchedulerConfig {
    pub total_live_fanout: usize,        // hard cap across all origins (default 8)
    pub per_origin_fanout: usize,        // default 4
    pub auto_promote_origins: usize,     // 2..=4 origins auto-promoted
    pub hot_max_age_secs: i64,           // e.g. 3600
    pub warm_max_age_secs: i64,          // e.g. 86400
    pub query_refresh_limit: usize,      // max P0 jobs spawned per query
}
pub struct ActivationScheduler { /* config, ledger, per-origin accounting */ }
impl ActivationScheduler {
    pub fn new(config: SchedulerConfig, ledger: BudgetLedger) -> Self;
    /// Query-triggered refresh: returns up to `query_refresh_limit` P0 activations for a
    /// time-sensitive query, charged against the ledger; empty for a query that is not
    /// time-sensitive. Must NOT exceed per-origin or total fan-out.
    pub fn on_query(&self, query: &str, candidates: &[(String, String)], now: i64) -> Vec<Activation>;
    /// Background due work: HOT before WARM before COLD; respects per-origin + total fan-out.
    pub fn due(&self, sources: &[SourceDue], now: i64) -> Vec<Activation>;
    pub fn in_flight(&self) -> usize;
}
pub struct SourceDue { pub origin: String, pub class: SourceClass, pub change_rate: f32,
                       pub last_fetch: i64, pub next_allowed_at: i64, pub url: String }

/// Is a query time-sensitive ("latest X", "X today", "news", year/date tokens)?
pub fn is_time_sensitive(query: &str, now: i64) -> bool;
```

Hard invariants (test them): total in-flight never exceeds `total_live_fanout`; no origin
exceeds `per_origin_fanout`; the ledger is charged before an activation is returned and a
refused charge yields no activation (no storm); P0 outranks P1..P4 in `due()` ordering.

## agent0 wiring (Stage 3 integration)

- Feed the crawl loop through the scheduler: background `due()` work and `on_query()` P0
  refresh. HOT sources refresh sooner than WARM than COLD, using `change_probability` and
  the existing conditional-refresh path (a 304 is still `Unchanged`).
- Live overlay: a newly fetched doc from a P0 refresh must be searchable immediately and
  scored against the active query, marked with overlay provenance, **before** any global
  reindex. Keep base segments immutable; merge by (overlay) watermark.
- Per-origin limits must hold under load — demonstrate with a scheduled burst test.

## Exit gate

A "latest/today" query returns a document the local index did not yet contain (fetched by a
P0 query-triggered refresh), and a burst of refresh work does not exceed the configured
per-origin or total fan-out. Demonstrated by a hermetic test with a fixture server that
records its request ordering.
