//! `search-planner` — optional, opt-in LLM query planning.
//!
//! Implements [`search_engine::QueryPlanner`] so a query-time model can deconstruct a question
//! into probes the engine then searches as §8 query variants, alongside the original, under the
//! §10 variant-family cap.
//!
//! It is a thin crate on purpose. The prompt and the response parser live in
//! `search-organize`'s `queryplan` module, which is the live-verified implementation; this crate
//! owns the *synchronous* engine seam (the engine is synchronous) and the mapping from that plan
//! onto the engine's probe list. A second prompt for the same job would be a second thing to keep
//! correct.
//!
//! Design guarantees, mirroring `search-answer`:
//!
//! * **Opt-in.** [`from_env`] returns `None` unless `SEARCH_PLANNER_ENABLED` is truthy *and*
//!   `KITANI_API_KEY` is set. The caller then passes no planner and search behaves exactly as
//!   before.
//! * **Never blocks or fails a search.** A transport error, a timeout, or an unparseable
//!   response becomes an empty plan plus a trace note; the engine ignores it.
//! * **Cached.** Planning goes through the shared `search-organize` client, so a repeated query
//!   costs no tokens and performs no network call.
//! * **The slow model by default.** The planner defaults to the reasoning model
//!   (`deepseek/deepseek-v4.1-flash-thinking`); set `SEARCH_PLANNER_MODEL` to trade latency for
//!   cost.

#![forbid(unsafe_code)]

use std::sync::Arc;

mod client;
mod config;

pub use client::{plan_to_probes, LlmQueryPlanner, PlannerError};
pub use config::{
    api_key, disabled_reason, is_enabled, Disabled, PlannerConfig, DEFAULT_CONCURRENCY,
    DEFAULT_MAX_EXPANSIONS, DEFAULT_PLANNER_MODEL, DEFAULT_TIMEOUT_S,
};

/// Build the planner from the environment, or `None` when it is disabled.
///
/// Returns `None` when `SEARCH_PLANNER_ENABLED` is unset/0, when `KITANI_API_KEY` is absent, or if
/// the client/runtime cannot be built — never panics, never reads a credential from a file. Use
/// [`disabled_reason`] for the single explanatory log line.
pub fn from_env() -> Option<LlmQueryPlanner> {
    let cfg = PlannerConfig::from_env().ok()?;
    LlmQueryPlanner::new(cfg).ok()
}

/// Test/injection variant of [`from_env`]: build a planner around a caller-supplied client
/// (for example one over a fake transport), with an explicit expansion cap.
pub fn with_client(
    client: Arc<search_organize::LlmClient>,
    max_expansions: usize,
) -> Option<LlmQueryPlanner> {
    LlmQueryPlanner::with_client(client, max_expansions).ok()
}
