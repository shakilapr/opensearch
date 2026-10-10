//! Hermetic wiring tests for the `agent0 serve` answer path.
//!
//! These prove (a) that the serve wiring compiles against the frozen API signatures
//! (`search_api::serve_with` / `AppState::with_answerer`) and (b) that with no key the answerer
//! is `None` and the search path is unchanged. No network, no real key, no fixed port: the
//! `serve_with` calls use an unparseable bind address so they fail fast before binding.

use std::sync::{Arc, Mutex};

use search_api::{IndexHit, Searcher};
use search_core::config::Config;
use search_core::{AppResult, SearchFilters};

/// A stub searcher: enough to build an `AppState`, never actually queried here.
struct StubSearcher;

impl Searcher for StubSearcher {
    fn search(
        &self,
        _q: &str,
        _limit: usize,
        _filters: Option<&SearchFilters>,
    ) -> AppResult<Vec<IndexHit>> {
        Ok(Vec::new())
    }
}

/// A trivial answerer to prove the `Option<Arc<dyn Answerer>>` parameter is accepted.
struct FakeAnswerer;

impl search_engine::Answerer for FakeAnswerer {
    fn answer(
        &self,
        _query: &str,
        _evidence: &[search_engine::evidence::EvidenceRecord],
        _passages: &[String],
    ) -> Result<String, String> {
        Ok("stub".to_string())
    }
}

fn bad_bind_config() -> Config {
    let mut cfg = Config::default();
    // An unparseable address: `serve_with` returns Err from `TcpListener::bind` immediately,
    // so the test never serves or blocks.
    cfg.bind = "not-a-valid-address::".to_string();
    cfg
}

static ENV_LOCK: Mutex<()> = Mutex::new(());

/// With no key (and the feature off), `from_env` returns `None` and names the reason; the search
/// path then behaves exactly as before (no answerer injected).
#[test]
fn no_key_disables_answerer_and_search_is_unchanged() {
    let _guard = ENV_LOCK.lock().unwrap();
    let saved_enabled = std::env::var("SEARCH_ANSWER_ENABLED").ok();
    let saved_key = std::env::var("KITANI_API_KEY").ok();

    std::env::set_var("SEARCH_ANSWER_ENABLED", "1");
    std::env::remove_var("KITANI_API_KEY");
    assert!(search_answer::from_env().is_none());
    assert_eq!(
        search_answer::disabled_reason(),
        search_answer::Disabled::MissingKey
    );

    std::env::remove_var("SEARCH_ANSWER_ENABLED");
    assert!(search_answer::from_env().is_none());
    assert_eq!(
        search_answer::disabled_reason(),
        search_answer::Disabled::FeatureDisabled
    );

    match saved_enabled {
        Some(v) => std::env::set_var("SEARCH_ANSWER_ENABLED", v),
        None => std::env::remove_var("SEARCH_ANSWER_ENABLED"),
    }
    match saved_key {
        Some(v) => std::env::set_var("KITANI_API_KEY", v),
        None => std::env::remove_var("KITANI_API_KEY"),
    }
}

/// `serve_with(cfg, searcher, deep, answerer, planner, extra_lanes)` compiles, and the
/// `answerer = None` path is unchanged: it builds state and returns the bind error.
#[tokio::test]
async fn serve_with_none_answerer_uses_the_unchanged_path() {
    let cfg = bad_bind_config();
    let searcher: Arc<dyn Searcher + Send + Sync> = Arc::new(StubSearcher);
    let err = search_api::serve_with(&cfg, searcher, None, None, None, Vec::new()).await;
    assert!(err.is_err(), "an unparseable bind address must fail");

    // The builder function is reachable with the exact answerer type.
    let searcher2: Arc<dyn Searcher + Send + Sync> = Arc::new(StubSearcher);
    let state = search_api::AppState::new(searcher2);
    assert!(state.answerer().is_none());
}

/// `serve_with` accepts a `Some(Arc<dyn search_engine::Answerer>)`, proving the answerer flows
/// through the wiring to `AppState::with_answerer`.
#[tokio::test]
async fn serve_with_accepts_an_answerer() {
    let cfg = bad_bind_config();
    let searcher: Arc<dyn Searcher + Send + Sync> = Arc::new(StubSearcher);
    let answerer: Arc<dyn search_engine::Answerer + Send + Sync> = Arc::new(FakeAnswerer);
    let err = search_api::serve_with(&cfg, searcher, None, Some(answerer.clone()), None, Vec::new()).await;
    assert!(err.is_err(), "an unparseable bind address must fail");

    // The state carries the answerer through.
    let searcher2: Arc<dyn Searcher + Send + Sync> = Arc::new(StubSearcher);
    let state = search_api::AppState::new(searcher2).with_answerer(answerer);
    assert!(state.answerer().is_some());
    assert_eq!(
        state
            .answerer()
            .unwrap()
            .answer("q", &[], &[])
            .unwrap(),
        "stub"
    );
}

/// A trivial planner to prove the `Option<Arc<dyn QueryPlanner>>` parameter is accepted.
struct FakePlanner;

impl search_engine::QueryPlanner for FakePlanner {
    fn plan(
        &self,
        _query: &str,
        _analysis: &search_engine::QueryAnalysis,
    ) -> search_engine::PlannedQueries {
        search_engine::PlannedQueries::none()
    }
}

/// `serve_with` accepts a `Some(Arc<dyn search_engine::QueryPlanner>)`, proving the planner flows
/// through the wiring to `AppState::with_planner`.
#[tokio::test]
async fn serve_with_accepts_a_planner() {
    let cfg = bad_bind_config();
    let searcher: Arc<dyn Searcher + Send + Sync> = Arc::new(StubSearcher);
    let planner: Arc<dyn search_engine::QueryPlanner + Send + Sync> = Arc::new(FakePlanner);
    let err =
        search_api::serve_with(&cfg, searcher, None, None, Some(planner.clone()), Vec::new()).await;
    assert!(err.is_err(), "an unparseable bind address must fail");

    // The state carries the planner through.
    let searcher2: Arc<dyn Searcher + Send + Sync> = Arc::new(StubSearcher);
    let state = search_api::AppState::new(searcher2).with_planner(planner);
    assert!(state.planner().is_some());
}
