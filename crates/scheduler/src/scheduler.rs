//! Activation scheduler: bounded per-origin / total live fan-out with a charge-before-dispatch
//! budget ledger (spec §40–41, engine-arch §13).
//!
//! Every [`Activation`] returned by [`ActivationScheduler::on_query`] or
//! [`ActivationScheduler::due`] has *first* charged the [`BudgetLedger`] and *then* reserved a
//! live-fetch slot (total and per origin). If either the charge or a slot is unavailable, no
//! activation is produced — this is what prevents a refresh storm. All accounting lives behind
//! interior mutability, so the `&self` methods are safe to call concurrently and the caps hold
//! under a burst.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::budget::BudgetLedger;
use crate::class::SourceClass;
use crate::priority::Priority;
use crate::time_sensitive::is_time_sensitive;

/// A single unit of refresh work the scheduler decides to run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Activation {
    /// URL to (re)fetch.
    pub url: String,
    /// Dispatch priority tier.
    pub priority: Priority,
    /// Why this activation was produced.
    pub reason: ActivationReason,
    /// Origin (scheme+host) the URL belongs to; fan-out is capped per origin.
    pub origin: String,
    /// Time (Unix seconds) at which the activation is deemed due.
    pub due_at: i64,
}

/// Why an activation was produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ActivationReason {
    /// Triggered by a time-sensitive query (P0).
    QueryCritical,
    /// A HOT source update lane.
    Changed,
    /// Normal background refresh (staleness schedule reached).
    Due,
    /// Discovery of new sources / links.
    Discovery,
    /// Archival / backfill of old material.
    Backfill,
}

/// Scheduler configuration. Defaults follow engine-arch §13 (2–4 concurrent origins in auto mode,
/// 8 total fetches as a hard cap, not 8 to one origin).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchedulerConfig {
    /// Hard cap on concurrent live fetches across all origins. Default `8`.
    pub total_live_fanout: usize,
    /// Hard cap on concurrent live fetches to a single origin. Default `4`.
    pub per_origin_fanout: usize,
    /// Number of origins auto-promoted to the live lane. Default `3`. (Caller policy hint; kept
    /// within the documented `2..=4` range.)
    pub auto_promote_origins: usize,
    /// A HOT source is considered stale once its last fetch is older than this. Default `3600`.
    pub hot_max_age_secs: i64,
    /// A WARM source is considered stale once its last fetch is older than this. Default `86400`.
    pub warm_max_age_secs: i64,
    /// Maximum number of P0 jobs a single query may spawn. Default `4`.
    pub query_refresh_limit: usize,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            total_live_fanout: 8,
            per_origin_fanout: 4,
            auto_promote_origins: 3,
            hot_max_age_secs: 3600,
            warm_max_age_secs: 86400,
            query_refresh_limit: 4,
        }
    }
}

impl SchedulerConfig {
    /// Schedule horizon (max age before a source of `class` is stale). HOT and WARM use their
    /// configured horizons; COLD uses a week (`warm_max_age_secs * 7`) as the archival horizon.
    /// Callers use this to compute `SourceDue::next_allowed_at`.
    pub fn max_age_secs(&self, class: SourceClass) -> i64 {
        match class {
            SourceClass::Hot => self.hot_max_age_secs,
            SourceClass::Warm => self.warm_max_age_secs,
            SourceClass::Cold => self.warm_max_age_secs.saturating_mul(7),
        }
    }
}

/// A background-refresh candidate offered to [`ActivationScheduler::due`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SourceDue {
    /// Origin (scheme+host) of the source.
    pub origin: String,
    /// Freshness class of the source.
    pub class: SourceClass,
    /// Learned change rate (changes per second).
    pub change_rate: f32,
    /// Last successful fetch (Unix seconds).
    pub last_fetch: i64,
    /// Earliest time (Unix seconds) the origin/source may be fetched again.
    pub next_allowed_at: i64,
    /// URL to refresh.
    pub url: String,
}

/// Mutable in-flight accounting, guarded as a unit so per-origin and total caps stay consistent.
#[derive(Debug, Default)]
struct InFlight {
    total: usize,
    per_origin: HashMap<String, usize>,
    urls: HashSet<String>,
}

/// Result of attempting to admit one activation.
enum Admit {
    Admitted,
    /// Same URL already in flight, or this origin is at its per-origin cap — skip this
    /// candidate and continue with the rest (an origin hitting its cap must not starve others).
    Skipped,
    /// The total fan-out cap or the budget is exhausted — stop the current pass.
    Stopped,
}

/// The activation scheduler. Methods take `&self`; in-flight accounting is interior-mutable.
#[derive(Debug)]
pub struct ActivationScheduler {
    config: SchedulerConfig,
    ledger: BudgetLedger,
    state: Mutex<InFlight>,
}

impl ActivationScheduler {
    /// Build a scheduler from a config and a budget ledger.
    pub fn new(config: SchedulerConfig, ledger: BudgetLedger) -> Self {
        Self {
            config,
            ledger,
            state: Mutex::new(InFlight::default()),
        }
    }

    /// The active configuration.
    pub fn config(&self) -> &SchedulerConfig {
        &self.config
    }

    /// Total live activations currently in flight.
    pub fn in_flight(&self) -> usize {
        self.state.lock().expect("scheduler state poisoned").total
    }

    /// Live activations currently in flight for a single origin.
    pub fn in_flight_for(&self, origin: &str) -> usize {
        self.state
            .lock()
            .expect("scheduler state poisoned")
            .per_origin
            .get(origin)
            .copied()
            .unwrap_or(0)
    }

    /// Query-triggered refresh. For a time-sensitive query, returns up to
    /// `query_refresh_limit` P0 activations drawn from `candidates` (`(url, origin)`), skipping
    /// any URL already in flight and stopping at the per-origin or total fan-out cap. Each
    /// returned activation has already charged the ledger. A non-time-sensitive query returns an
    /// empty vector and charges nothing.
    pub fn on_query(
        &self,
        query: &str,
        candidates: &[(String, String)],
        now: i64,
    ) -> Vec<Activation> {
        let mut out = Vec::new();
        if !is_time_sensitive(query, now) {
            return out;
        }
        let limit = self.config.query_refresh_limit;
        for (url, origin) in candidates {
            if out.len() >= limit {
                break;
            }
            match self.admit(url, origin) {
                Admit::Admitted => out.push(Activation {
                    url: url.clone(),
                    priority: Priority::P0QueryCritical,
                    reason: ActivationReason::QueryCritical,
                    origin: origin.clone(),
                    due_at: now,
                }),
                Admit::Skipped => continue,
                Admit::Stopped => break,
            }
        }
        out
    }

    /// Background due work: considers sources whose `next_allowed_at <= now`, dispatched HOT
    /// before WARM before COLD (input order preserved within a class), subject to the per-origin
    /// and total fan-out caps. Each returned activation has already charged the ledger.
    pub fn due(&self, sources: &[SourceDue], now: i64) -> Vec<Activation> {
        let mut eligible: Vec<&SourceDue> =
            sources.iter().filter(|s| s.next_allowed_at <= now).collect();
        // SourceClass derives Ord as Hot < Warm < Cold; stable sort keeps input order per class.
        eligible.sort_by_key(|s| s.class);

        let mut out = Vec::new();
        for s in eligible {
            match self.admit(&s.url, &s.origin) {
                Admit::Admitted => out.push(Activation {
                    url: s.url.clone(),
                    priority: priority_for(s.class),
                    reason: reason_for(s.class),
                    origin: s.origin.clone(),
                    due_at: now,
                }),
                Admit::Skipped => continue,
                Admit::Stopped => break,
            }
        }
        out
    }

    /// Mark an in-flight activation complete, releasing its fan-out slot (and any unspent
    /// budget). Called by the crawl loop in `agent0` when a fetch finishes.
    pub fn release(&self, url: &str, origin: &str) {
        let mut st = self.state.lock().expect("scheduler state poisoned");
        if !st.urls.remove(url) {
            return;
        }
        st.total = st.total.saturating_sub(1);
        if let Some(n) = st.per_origin.get_mut(origin) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                st.per_origin.remove(origin);
            }
        }
    }

    /// Charge the ledger and reserve a live-fetch slot for `url` at `origin`, or explain why not.
    fn admit(&self, url: &str, origin: &str) -> Admit {
        let mut st = self.state.lock().expect("scheduler state poisoned");
        if st.urls.contains(url) {
            return Admit::Skipped;
        }
        if st.total >= self.config.total_live_fanout {
            return Admit::Stopped;
        }
        if st.per_origin.get(origin).copied().unwrap_or(0) >= self.config.per_origin_fanout {
            return Admit::Skipped;
        }
        // Charge BEFORE reserving/returning: a refused charge yields no activation.
        if !self.ledger.try_charge(1) {
            return Admit::Stopped;
        }
        st.total += 1;
        *st.per_origin.entry(origin.to_string()).or_insert(0) += 1;
        st.urls.insert(url.to_string());
        Admit::Admitted
    }
}

fn priority_for(class: SourceClass) -> Priority {
    match class {
        SourceClass::Hot => Priority::P1Hot,
        SourceClass::Warm => Priority::P2Normal,
        SourceClass::Cold => Priority::P4Backfill,
    }
}

fn reason_for(class: SourceClass) -> ActivationReason {
    match class {
        SourceClass::Hot => ActivationReason::Changed,
        SourceClass::Warm => ActivationReason::Due,
        SourceClass::Cold => ActivationReason::Backfill,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn sched(cfg: SchedulerConfig, capacity: u64) -> ActivationScheduler {
        ActivationScheduler::new(cfg, BudgetLedger::new(capacity))
    }

    fn cands(origin: &str, urls: &[&str]) -> Vec<(String, String)> {
        urls.iter()
            .map(|u| (u.to_string(), origin.to_string()))
            .collect()
    }

    fn src(origin: &str, url: &str, class: SourceClass, next_allowed_at: i64) -> SourceDue {
        SourceDue {
            origin: origin.to_string(),
            class,
            change_rate: 0.0,
            last_fetch: 0,
            next_allowed_at,
            url: url.to_string(),
        }
    }

    #[test]
    fn on_query_time_sensitive_returns_bounded_p0() {
        let mut cfg = SchedulerConfig::default();
        cfg.query_refresh_limit = 2;
        let s = sched(cfg, 100);
        let c = cands(
            "https://news.test",
            &[
                "https://news.test/a",
                "https://news.test/b",
                "https://news.test/c",
                "https://news.test/d",
            ],
        );
        let acts = s.on_query("latest rust news", &c, 1_781_481_600);
        assert_eq!(acts.len(), 2, "must return at most query_refresh_limit");
        for a in &acts {
            assert_eq!(a.priority, Priority::P0QueryCritical);
            assert_eq!(a.reason, ActivationReason::QueryCritical);
            assert_eq!(a.origin, "https://news.test");
        }
        assert_eq!(s.in_flight(), 2);
        // Each returned activation charged the ledger.
        assert_eq!(s.ledger.spent(), 2);
    }

    #[test]
    fn on_query_plain_query_is_empty() {
        let s = sched(SchedulerConfig::default(), 100);
        let c = cands("https://x.test", &["https://x.test/a"]);
        let acts = s.on_query("rust ownership model", &c, 1_781_481_600);
        assert!(acts.is_empty());
        assert_eq!(s.in_flight(), 0);
        assert_eq!(s.ledger.spent(), 0, "a non-sensitive query charges nothing");
    }

    #[test]
    fn on_query_skips_in_flight_duplicates() {
        let s = sched(SchedulerConfig::default(), 100);
        let c = cands("https://x.test", &["https://x.test/a", "https://x.test/b"]);
        let first = s.on_query("today", &c, 0);
        assert_eq!(first.len(), 2);
        // The same URLs are already in flight: nothing further is admitted or charged.
        let second = s.on_query("today", &c, 0);
        assert!(second.is_empty());
        assert_eq!(s.ledger.spent(), 2);
    }

    #[test]
    fn on_query_stops_at_total_and_per_origin_caps() {
        let mut cfg = SchedulerConfig::default();
        cfg.query_refresh_limit = 10;
        cfg.total_live_fanout = 3;
        cfg.per_origin_fanout = 2;
        let s = sched(cfg, 100);
        let c = cands(
            "https://a.test",
            &[
                "https://a.test/1",
                "https://a.test/2",
                "https://a.test/3",
                "https://a.test/4",
            ],
        );
        let acts = s.on_query("breaking news", &c, 0);
        // per_origin_fanout = 2 bounds this single origin.
        assert_eq!(acts.len(), 2);
        assert_eq!(s.in_flight_for("https://a.test"), 2);
        assert!(s.in_flight() <= 3);
    }

    #[test]
    fn due_orders_hot_before_warm_before_cold() {
        let cfg = SchedulerConfig::default();
        let s = sched(cfg, 100);
        // Deliberately supplied out of order; one COLD entry is not yet allowed.
        let sources = vec![
            src("https://w.test", "https://w.test/1", SourceClass::Warm, 0),
            src("https://c.test", "https://c.test/1", SourceClass::Cold, 0),
            src("https://h.test", "https://h.test/1", SourceClass::Hot, 0),
            src("https://h2.test", "https://h2.test/1", SourceClass::Hot, 1_000),
        ];
        let acts = s.due(&sources, 500);
        let classes: Vec<SourceClass> = acts
            .iter()
            .map(|a| match a.origin.as_str() {
                "https://h.test" | "https://h2.test" => SourceClass::Hot,
                "https://w.test" => SourceClass::Warm,
                _ => SourceClass::Cold,
            })
            .collect();
        // The future-dated HOT source is excluded because next_allowed_at > now.
        assert_eq!(classes, vec![SourceClass::Hot, SourceClass::Warm, SourceClass::Cold]);
        assert_eq!(acts[0].priority, Priority::P1Hot);
        assert_eq!(acts[1].priority, Priority::P2Normal);
        assert_eq!(acts[2].priority, Priority::P4Backfill);
        assert_eq!(s.in_flight(), 3);
        assert_eq!(s.ledger.spent(), 3);
    }

    #[test]
    fn due_respects_next_allowed_at() {
        let s = sched(SchedulerConfig::default(), 100);
        let sources = vec![
            src("https://a.test", "https://a.test/1", SourceClass::Hot, 900),
            src("https://b.test", "https://b.test/1", SourceClass::Warm, 1_000),
            src("https://c.test", "https://c.test/1", SourceClass::Cold, 2_000),
        ];
        let acts = s.due(&sources, 1_000);
        let urls: Vec<&str> = acts.iter().map(|a| a.url.as_str()).collect();
        assert_eq!(urls, vec!["https://a.test/1", "https://b.test/1"]);
    }

    #[test]
    fn burst_never_exceeds_total_fanout() {
        let mut cfg = SchedulerConfig::default();
        cfg.total_live_fanout = 4;
        cfg.per_origin_fanout = 100; // non-binding here
        let s = sched(cfg.clone(), 10_000);
        let mut sources = Vec::new();
        for i in 0..10 {
            let o = format!("https://o{i}.test");
            for j in 0..5 {
                sources.push(src(&o, &format!("{o}/{j}"), SourceClass::Hot, 0));
            }
        }
        let mut admitted = 0;
        for _ in 0..50 {
            admitted += s.due(&sources, 10_000).len();
        }
        assert_eq!(admitted, 4, "exactly total_live_fanout admissions possible");
        assert_eq!(s.in_flight(), 4);
        assert!(s.in_flight() <= cfg.total_live_fanout);
    }

    #[test]
    fn burst_never_exceeds_per_origin_fanout() {
        let mut cfg = SchedulerConfig::default();
        cfg.total_live_fanout = 100; // non-binding here
        cfg.per_origin_fanout = 3;
        let s = sched(cfg.clone(), 10_000);
        let mut sources = Vec::new();
        let origins = ["https://a.test", "https://b.test"];
        for o in origins {
            for j in 0..10 {
                sources.push(src(o, &format!("{o}/{j}"), SourceClass::Warm, 0));
            }
        }
        let mut per_origin: HashMap<String, usize> = HashMap::new();
        for _ in 0..30 {
            for a in s.due(&sources, 10_000) {
                *per_origin.entry(a.origin).or_insert(0) += 1;
            }
        }
        for o in origins {
            assert_eq!(per_origin.get(o).copied().unwrap_or(0), 3);
            assert_eq!(s.in_flight_for(o), 3);
            assert!(s.in_flight_for(o) <= cfg.per_origin_fanout);
        }
        assert!(s.in_flight() <= cfg.total_live_fanout);
    }

    #[test]
    fn concurrent_burst_caps_hold() {
        let mut cfg = SchedulerConfig::default();
        cfg.total_live_fanout = 8;
        cfg.per_origin_fanout = 3;
        cfg.query_refresh_limit = 20;
        let s = Arc::new(sched(cfg.clone(), 100_000));

        let mut handles = Vec::new();
        for i in 0..8 {
            let s = Arc::clone(&s);
            handles.push(std::thread::spawn(move || {
                let origin = format!("https://t{i}.test");
                let cands: Vec<(String, String)> = (0..10)
                    .map(|j| (format!("{origin}/{j}"), origin.clone()))
                    .collect();
                for _ in 0..100 {
                    s.on_query("latest news today", &cands, 0);
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert!(s.in_flight() <= cfg.total_live_fanout);
        for i in 0..8 {
            assert!(s.in_flight_for(&format!("https://t{i}.test")) <= cfg.per_origin_fanout);
        }
        // Saturated to the global cap, per-origin counts never exceed their cap.
        assert_eq!(s.in_flight(), cfg.total_live_fanout);
    }

    #[test]
    fn release_frees_slots() {
        let mut cfg = SchedulerConfig::default();
        cfg.per_origin_fanout = 1;
        let s = sched(cfg, 100);
        let a = cands("https://x.test", &["https://x.test/a"]);
        assert_eq!(s.on_query("today", &a, 0).len(), 1);
        // Origin is saturated.
        assert!(s.on_query("today", &a, 0).is_empty());
        s.release("https://x.test/a", "https://x.test");
        assert_eq!(s.in_flight_for("https://x.test"), 0);
        let b = cands("https://x.test", &["https://x.test/b"]);
        let again = s.on_query("today", &b, 0);
        assert_eq!(again.len(), 1);
        assert_eq!(again[0].url, "https://x.test/b");
    }
}
