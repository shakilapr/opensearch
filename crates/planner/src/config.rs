//! Planner configuration.
//!
//! Every knob is optional and read from the environment. The credential is read **only** from
//! `KITANI_API_KEY` (the same variable `search-organize` and `search-answer` use) and is never
//! hardcoded, logged, or written anywhere.
//!
//! The default model is the **slow reasoning** one (`deepseek/deepseek-v4.1-flash-thinking`).
//! Deconstructing a query into entity/phrase probes is the kind of task where the thinking model
//! is worth the latency, and the on-disk cache means a given query is only ever paid for once.
//! Set `SEARCH_PLANNER_MODEL` to the cheap model when the extra latency is not wanted.

use std::path::PathBuf;

use search_organize::{LlmConfig, DEFAULT_BASE_URL};

/// Default planner model: the slow reasoning model, deliberately.
pub const DEFAULT_PLANNER_MODEL: &str = "deepseek/deepseek-v4.1-flash-thinking";
/// Default per-request timeout, seconds. The reasoning model is slow on a long prompt.
pub const DEFAULT_TIMEOUT_S: u64 = 180;
/// Default bound on simultaneous in-flight planning calls.
pub const DEFAULT_CONCURRENCY: usize = 2;
/// Default cap on expansions returned per query. `plan::MAX_QUERY_VARIANTS` is 4 including the
/// original, so three is the most the engine can even use.
pub const DEFAULT_MAX_EXPANSIONS: usize = 3;

/// Why the planner is disabled. Reported once by the serve wiring.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disabled {
    /// `SEARCH_PLANNER_ENABLED` is unset/0, so planning is opt-in.
    FeatureDisabled,
    /// The feature is requested but `KITANI_API_KEY` is absent/empty.
    MissingKey,
}

impl Disabled {
    /// A short, non-secret human message for the single info log line.
    pub fn message(self) -> &'static str {
        match self {
            Disabled::FeatureDisabled => {
                "query planning disabled (SEARCH_PLANNER_ENABLED is unset or 0)"
            }
            Disabled::MissingKey => {
                "query planning disabled (no API key: SEARCH_PLANNER_API_KEY / SEARCH_LLM_API_KEY / KITANI_API_KEY)"
            }
        }
    }
}

/// Resolved planner configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannerConfig {
    /// The underlying LLM client configuration (base URL, model, timeout, key).
    pub llm: LlmConfig,
    /// Directory holding the on-disk plan cache.
    pub cache_dir: PathBuf,
    /// Bound on simultaneous in-flight planning calls.
    pub concurrency: usize,
    /// Cap on expansions returned per query.
    pub max_expansions: usize,
}

fn env_trim(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Whether query planning is opted in (`SEARCH_PLANNER_ENABLED` truthy).
pub fn is_enabled() -> bool {
    match env_trim("SEARCH_PLANNER_ENABLED").map(|s| s.to_ascii_lowercase()) {
        Some(v) => matches!(v.as_str(), "1" | "true" | "on" | "yes"),
        None => false,
    }
}

/// Split a comma-separated model list into ids, dropping blanks.
fn parse_models(listed: &str) -> Vec<String> {
    listed
        .split(',')
        .map(|m| m.trim().to_string())
        .filter(|m| !m.is_empty())
        .collect()
}

/// The API key. `SEARCH_PLANNER_API_KEY`, else the shared `SEARCH_LLM_API_KEY` /
/// `OPENROUTER_API_KEY`, else `KITANI_API_KEY`. Never logged or persisted.
///
/// The shared names come first because the planner is part of the engine: when the engine rides
/// one provider chain, the planner must ride the same one, or it plans against a dead vendor.
pub fn api_key() -> Option<String> {
    env_trim("SEARCH_PLANNER_API_KEY")
        .or_else(|| env_trim("SEARCH_LLM_API_KEY"))
        .or_else(|| env_trim("OPENROUTER_API_KEY"))
        .or_else(|| env_trim("KITANI_API_KEY"))
}

/// The reason the planner is disabled. Only meaningful when [`PlannerConfig::from_env`] returns
/// `Err` / [`crate::from_env`] returns `None`.
pub fn disabled_reason() -> Disabled {
    if !is_enabled() {
        Disabled::FeatureDisabled
    } else {
        Disabled::MissingKey
    }
}

/// Resolve the cache directory: `SEARCH_PLANNER_CACHE`, else a `planner` subdirectory of the
/// shared LLM cache (`SEARCH_LLM_CACHE`, else `SEARCH_DATA_DIR/llm-cache`, else
/// `/var/lib/search/llm-cache`). A distinct subdirectory keeps plan entries from interleaving
/// with organize/answer entries that share the parent.
fn resolve_cache_dir() -> PathBuf {
    if let Some(dir) = env_trim("SEARCH_PLANNER_CACHE") {
        return PathBuf::from(dir);
    }
    if let Some(dir) = env_trim("SEARCH_LLM_CACHE") {
        return PathBuf::from(dir).join("planner");
    }
    let data = env_trim("SEARCH_DATA_DIR").unwrap_or_else(|| "/var/lib/search".to_string());
    PathBuf::from(data).join("llm-cache").join("planner")
}

impl PlannerConfig {
    /// Build the configuration from the environment.
    ///
    /// Returns `Err(Disabled::FeatureDisabled)` when `SEARCH_PLANNER_ENABLED` is unset/0 and
    /// `Err(Disabled::MissingKey)` when the feature is on but `KITANI_API_KEY` is absent.
    pub fn from_env() -> Result<PlannerConfig, Disabled> {
        if !is_enabled() {
            return Err(Disabled::FeatureDisabled);
        }
        let api_key = api_key().ok_or(Disabled::MissingKey)?;

        // The planner rides the engine's provider chain when one is configured (`SEARCH_LLM_*`), so
        // the engine does not talk to two vendors with one model list; the planner-specific
        // variables still win, then the vendor's own.
        let shared_base_url = env_trim("SEARCH_LLM_BASE_URL");
        let base_url = env_trim("SEARCH_PLANNER_BASE_URL")
            .or_else(|| shared_base_url.clone())
            .or_else(|| env_trim("KITANI_BASE_URL"))
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_string())
            .trim_end_matches('/')
            .to_string();
        let model = env_trim("SEARCH_PLANNER_MODEL")
            .or_else(|| env_trim("SEARCH_LLM_MODEL"))
            .or_else(|| env_trim("KITANI_MODEL"))
            .unwrap_or_else(|| DEFAULT_PLANNER_MODEL.to_string());
        // Inherit the shared model list only when the planner actually points at the shared
        // provider: the ids in it name models of *that* vendor, so sending them anywhere else is a
        // guaranteed 404 per model.
        let models = match env_trim("SEARCH_PLANNER_MODELS") {
            Some(listed) => parse_models(&listed),
            None => match shared_base_url.as_deref() {
                Some(shared) if base_url == shared.trim_end_matches('/') => {
                    search_organize::model_chain()
                }
                _ => Vec::new(),
            },
        };
        let timeout_s = env_trim("SEARCH_PLANNER_TIMEOUT_S")
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(DEFAULT_TIMEOUT_S);
        let concurrency = env_trim("SEARCH_PLANNER_CONCURRENCY")
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(DEFAULT_CONCURRENCY);
        let max_expansions = env_trim("SEARCH_PLANNER_MAX_EXPANSIONS")
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(DEFAULT_MAX_EXPANSIONS);

        Ok(PlannerConfig {
            llm: LlmConfig {
                base_url,
                // `LlmClient::complete` asks `model`; the escalate slot is unused here, so it
                // mirrors `model` rather than leaving a misleading second id.
                escalate_model: model.clone(),
                model,
                models,
                api_key: Some(api_key),
                timeout_s,
            },
            cache_dir: resolve_cache_dir(),
            concurrency,
            max_expansions,
        })
    }
}
