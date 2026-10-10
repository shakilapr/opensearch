//! Speculative planning: the request path never waits for a model.
//!
//! A model call cannot be microseconds — it is a network round trip to a reasoning model (measured
//! cold at 83 s on the live host). So the only way to have both the model's deconstruction *and* a
//! microsecond-scale request path is to take planning off the path entirely and run it in parallel:
//!
//! * **Hit** — the plan for this exact query is in the in-process map: a `HashMap` lookup, no lock
//!   contention beyond one mutex, no disk, no network. Microseconds.
//! * **Miss** — return *immediately* (the engine searches the original query, exactly as it would
//!   with no planner) and spawn the planning on a dedicated background runtime. Every miss also
//!   short-waits up to `SEARCH_PLANNER_WAIT_US` (default 3000 µs) on a condvar, so a plan that
//!   lands while the request is still in hand is used; a plan that takes longer improves the *next*
//!   request for the same query instead of this one.
//! * **In flight** — a second request for a query already being planned does not start a second
//!   call; it waits on the same condvar and shares the result. That is the concurrency control: N
//!   simultaneous identical queries cost one model call.
//!
//! Why this shape and not "block for the model": blocking is what made the first call 83 s, and the
//! engine's own contract says a planner returning nothing changes no behaviour — so a miss is a
//! clean, fast degradation to the deterministic path, not an error. Quality arrives on the second
//! identical query, which for a search box (and for an agent issuing a burst of related queries) is
//! nearly always within the same session.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use search_engine::analyze::QueryAnalysis;
use search_engine::planner::{PlannedQueries, QueryPlanner};

/// Counters, so the behaviour is observable rather than asserted.
#[derive(Debug, Default)]
pub struct PlannerStats {
    pub hits: AtomicU64,
    pub misses: AtomicU64,
    pub waited: AtomicU64,
    pub planned: AtomicU64,
    pub failed: AtomicU64,
}

/// A planner wrapped so that no request waits for it.
pub struct SpeculativePlanner {
    inner: Arc<dyn QueryPlanner + Send + Sync>,
    state: Arc<State>,
    runtime: tokio::runtime::Runtime,
    wait: Duration,
    pub stats: Arc<PlannerStats>,
}

#[derive(Default)]
struct State {
    cache: Mutex<HashMap<String, PlannedQueries>>,
    /// The condvar is notified whenever `cache` gains an entry.
    ready: Condvar,
    inflight: Mutex<std::collections::HashSet<String>>,
}

impl SpeculativePlanner {
    /// Wrap `inner`. `wait` bounds how long a miss may wait for a plan that is already being
    /// computed: pass `Duration::ZERO` for a strictly microsecond path.
    pub fn new(inner: Arc<dyn QueryPlanner + Send + Sync>, wait: Duration) -> Option<Self> {
        // A dedicated background runtime: the caller may be a synchronous trait method inside the
        // server's runtime, and planning must not compete with request handling for its threads.
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("query-planner")
            .enable_all()
            .build()
            .ok()?;
        Some(Self {
            inner,
            state: Arc::new(State::default()),
            runtime,
            wait,
            stats: Arc::new(PlannerStats::default()),
        })
    }

    /// Wait from the environment, in microseconds (`SEARCH_PLANNER_WAIT_US`).
    pub fn wait_from_env() -> Duration {
        let us = std::env::var("SEARCH_PLANNER_WAIT_US")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(3000);
        Duration::from_micros(us)
    }

    fn spawn_plan(&self, query: String, analysis: QueryAnalysis) {
        let inner = self.inner.clone();
        let state = self.state.clone();
        let stats = self.stats.clone();
        // A blocking task: `QueryPlanner::plan` is synchronous by contract and owns its blocking,
        // so it must not sit on an async worker's scheduler.
        self.runtime.spawn_blocking(move || {
            let planned = inner.plan(&query, &analysis);
            let ok = !planned.is_empty();
            let mut cache = match state.cache.lock() {
                Ok(c) => c,
                Err(_) => return,
            };
            cache.insert(query.clone(), planned);
            drop(cache);
            state.ready.notify_all();
            if let Ok(mut inflight) = state.inflight.lock() {
                inflight.remove(&query);
            }
            stats.planned.fetch_add(u64::from(ok), Ordering::Relaxed);
            if !ok {
                stats.failed.fetch_add(1, Ordering::Relaxed);
            }
        });
    }
}

impl QueryPlanner for SpeculativePlanner {
    fn plan(&self, query: &str, analysis: &QueryAnalysis) -> PlannedQueries {
        let key = query.trim().to_string();
        if key.is_empty() {
            return PlannedQueries::none();
        }

        // Fast path: an in-process map lookup. This is the microsecond case.
        if let Ok(cache) = self.state.cache.lock() {
            if let Some(planned) = cache.get(&key) {
                self.stats.hits.fetch_add(1, Ordering::Relaxed);
                let mut hit = planned.clone();
                hit.notes.push("plan served from the in-process cache".to_string());
                return hit;
            }
        }
        self.stats.misses.fetch_add(1, Ordering::Relaxed);

        // Start planning if nobody else has, then wait only as long as allowed.
        let already_inflight = match self.state.inflight.lock() {
            Ok(mut inflight) => !inflight.insert(key.clone()),
            Err(_) => false,
        };
        if !already_inflight {
            self.spawn_plan(key.clone(), analysis.clone());
        }

        if !self.wait.is_zero() {
            let deadline = Instant::now() + self.wait;
            if let Ok(cache) = self.state.cache.lock() {
                let mut cache = cache;
                while !cache.contains_key(&key) {
                    let now = Instant::now();
                    if now >= deadline {
                        break;
                    }
                    let (guard, timeout) = match self.state.ready.wait_timeout(cache, deadline - now) {
                        Ok(pair) => pair,
                        Err(_) => return PlannedQueries::none(),
                    };
                    cache = guard;
                    if timeout.timed_out() {
                        break;
                    }
                }
                if let Some(planned) = cache.get(&key) {
                    self.stats.waited.fetch_add(1, Ordering::Relaxed);
                    return planned.clone();
                }
            }
        }

        // Nothing within the budget: search the original query, exactly as with no planner at all.
        PlannedQueries {
            expansions: Vec::new(),
            notes: vec![format!(
                "planning in flight; this request searched the original query (a repeat is served from \
                 the in-process cache in microseconds)"
            )],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    /// A planner that counts calls and returns a fixed plan — the model, without the model.
    struct CountingPlanner {
        calls: Arc<AtomicUsize>,
        delay_ms: u64,
    }

    impl QueryPlanner for CountingPlanner {
        fn plan(&self, _query: &str, _analysis: &QueryAnalysis) -> PlannedQueries {
            self.calls.fetch_add(1, Ordering::Relaxed);
            std::thread::sleep(std::time::Duration::from_millis(self.delay_ms));
            PlannedQueries {
                expansions: vec!["+zeta +quokka".to_string()],
                notes: vec!["fake model".to_string()],
            }
        }
    }

    fn analysis() -> QueryAnalysis {
        search_engine::analyze::analyze_query("zeta quokka", 1_700_000_000, 0)
    }

    #[test]
    fn a_cold_query_does_not_wait_for_the_model() {
        // The whole point: the request path must not pay the model's latency.
        let calls = Arc::new(AtomicUsize::new(0));
        let planner = SpeculativePlanner::new(
            Arc::new(CountingPlanner { calls: calls.clone(), delay_ms: 300 }),
            Duration::ZERO,
        )
        .expect("runtime");

        let started = std::time::Instant::now();
        let planned = planner.plan("zeta quokka", &analysis());
        let elapsed = started.elapsed();

        assert!(planned.is_empty(), "a cold query searches the original: {planned:?}");
        assert!(
            elapsed < Duration::from_millis(50),
            "a cold query must return immediately, took {elapsed:?}"
        );
        // The plan is being computed in parallel, not skipped.
        std::thread::sleep(Duration::from_millis(400));
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn a_repeat_is_served_from_the_cache_with_the_plan() {
        let calls = Arc::new(AtomicUsize::new(0));
        let planner = SpeculativePlanner::new(
            Arc::new(CountingPlanner { calls: calls.clone(), delay_ms: 200 }),
            Duration::ZERO,
        )
        .expect("runtime");

        let _ = planner.plan("zeta quokka", &analysis());
        std::thread::sleep(Duration::from_millis(350));

        let started = std::time::Instant::now();
        let planned = planner.plan("zeta quokka", &analysis());
        let elapsed = started.elapsed();

        assert_eq!(planned.expansions, vec!["+zeta +quokka".to_string()]);
        assert!(elapsed < Duration::from_millis(5), "a hit must be microseconds, took {elapsed:?}");
        assert_eq!(calls.load(Ordering::Relaxed), 1, "the model is called exactly once");
        assert_eq!(planner.stats.hits.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn simultaneous_identical_queries_share_one_model_call() {
        // Concurrency control: a burst of the same query must not become a burst of model calls.
        let calls = Arc::new(AtomicUsize::new(0));
        let planner = Arc::new(
            SpeculativePlanner::new(
                Arc::new(CountingPlanner { calls: calls.clone(), delay_ms: 250 }),
                Duration::from_millis(50),
            )
            .expect("runtime"),
        );

        let mut handles = Vec::new();
        for _ in 0..8 {
            let p = planner.clone();
            handles.push(std::thread::spawn(move || p.plan("zeta quokka", &analysis())));
        }
        for h in handles {
            let _ = h.join();
        }
        std::thread::sleep(Duration::from_millis(350));
        assert_eq!(calls.load(Ordering::Relaxed), 1, "eight requests, one model call");
    }

    #[test]
    fn a_short_wait_catches_a_plan_that_lands_in_time() {
        // With a budget, a fast plan is used by the request that triggered it.
        let calls = Arc::new(AtomicUsize::new(0));
        let planner = SpeculativePlanner::new(
            Arc::new(CountingPlanner { calls: calls.clone(), delay_ms: 20 }),
            Duration::from_millis(500),
        )
        .expect("runtime");
        let planned = planner.plan("zeta quokka", &analysis());
        assert_eq!(planned.expansions, vec!["+zeta +quokka".to_string()]);
        assert_eq!(planner.stats.waited.load(Ordering::Relaxed), 1);
    }
}
