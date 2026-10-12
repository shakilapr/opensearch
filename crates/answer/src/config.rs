//! Answerer configuration.
//!
//! Every knob is optional and read from the environment. The credential is read **only** from the
//! environment and is never hardcoded, logged, or written anywhere. The vendor-neutral
//! `SEARCH_LLM_*` variables take precedence over the vendor-specific `SEARCH_ANSWER_*` /
//! `KITANI_API_KEY` ones, which remain the fallback.
//!
//! Defaults are chosen for the search hot path: a short, bounded timeout (20 s) and a small
//! token budget (1024). A reasoning model (id containing `thinking`/`reason`/`r1`/`o1`) needs a
//! much larger budget and a long timeout, so those two values are raised to safe minimums when
//! such a model is selected — an explicit opt-in to a slower call.

use std::path::PathBuf;

/// Default base URL for the OpenAI-chat-compatible provider.
pub const DEFAULT_BASE_URL: &str = "https://api.kitani.ai/v1";
/// Default answer model id.
pub const DEFAULT_MODEL: &str = "deepseek/deepseek-v4.1-flash";
/// Default per-request timeout in milliseconds (the answer is on the search hot path).
pub const DEFAULT_TIMEOUT_MS: u64 = 20_000;
/// Default `max_tokens` for the answer completion.
pub const DEFAULT_MAX_TOKENS: u32 = 1024;
/// Default bound on simultaneous in-flight transport calls.
pub const DEFAULT_CONCURRENCY: usize = 4;
/// Minimum `max_tokens` for a reasoning model (it spends budget on a hidden trace first).
pub const REASONING_MIN_TOKENS: u32 = 8192;
/// Minimum timeout (ms) for a reasoning model.
pub const REASONING_TIMEOUT_MS: u64 = 120_000;
/// Default base URL of the **fallback** provider (cun.ai), tried only when a primary call fails.
pub const DEFAULT_FALLBACK_BASE_URL: &str = "https://www.cun.ai/v1";
/// Default **fallback** model id (cun.ai's DeepSeek-V4.1-Flash).
pub const DEFAULT_FALLBACK_MODEL: &str = "DeepSeek-V4.1-Flash";

/// Why the answerer is disabled. Reported once by the serve wiring.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disabled {
    /// `SEARCH_ANSWER_ENABLED` is unset/0, so answer generation is opt-out.
    FeatureDisabled,
    /// The feature is requested but neither `SEARCH_LLM_API_KEY` nor `KITANI_API_KEY` is set.
    MissingKey,
}

impl Disabled {
    /// A short, non-secret human message for the single info log line.
    pub fn message(self) -> &'static str {
        match self {
            Disabled::FeatureDisabled => {
                "answer generation disabled (SEARCH_ANSWER_ENABLED is unset or 0)"
            }
            Disabled::MissingKey => {
                "answer generation disabled (SEARCH_LLM_API_KEY / KITANI_API_KEY is not set)"
            }
        }
    }
}

/// Resolved answerer configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnswerConfig {
    /// Base URL of the OpenAI-chat-compatible endpoint (no trailing `/chat/completions`).
    pub base_url: String,
    /// Model id to ask.
    pub model: String,
    /// Per-request HTTP timeout in milliseconds.
    pub timeout_ms: u64,
    /// `max_tokens` for the completion.
    pub max_tokens: u32,
    /// Bound on simultaneous in-flight transport calls.
    pub concurrency: usize,
    /// Directory holding the on-disk answer cache.
    pub cache_dir: PathBuf,
}

fn env_trim(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Whether answer generation is opted in (`SEARCH_ANSWER_ENABLED` truthy).
pub fn is_enabled() -> bool {
    match env_trim("SEARCH_ANSWER_ENABLED").map(|s| s.to_ascii_lowercase()) {
        Some(v) => matches!(v.as_str(), "1" | "true" | "on" | "yes"),
        None => false,
    }
}

/// The API key, read from `SEARCH_LLM_API_KEY` (preferred) or `KITANI_API_KEY`. Never logged or
/// persisted, and never read from a file.
pub fn api_key() -> Option<String> {
    env_trim("SEARCH_LLM_API_KEY").or_else(|| env_trim("KITANI_API_KEY"))
}

/// The fallback provider's base URL: `SEARCH_LLM_FALLBACK_BASE_URL`, else
/// [`DEFAULT_FALLBACK_BASE_URL`].
pub fn fallback_base_url() -> String {
    env_trim("SEARCH_LLM_FALLBACK_BASE_URL")
        .unwrap_or_else(|| DEFAULT_FALLBACK_BASE_URL.to_string())
}

/// The fallback provider's model id: `SEARCH_LLM_FALLBACK_MODEL`, else [`DEFAULT_FALLBACK_MODEL`].
pub fn fallback_model() -> String {
    env_trim("SEARCH_LLM_FALLBACK_MODEL").unwrap_or_else(|| DEFAULT_FALLBACK_MODEL.to_string())
}

/// The fallback credential: `SEARCH_LLM_FALLBACK_API_KEY`, else `CUN_API_KEY`.
///
/// `None` **disables** the fallback: the primary provider is then the only one tried. Read from
/// the environment only, never from a file, never logged.
pub fn fallback_api_key() -> Option<String> {
    env_trim("SEARCH_LLM_FALLBACK_API_KEY").or_else(|| env_trim("CUN_API_KEY"))
}

/// The reason the answerer is disabled. Only meaningful when [`AnswerConfig::from_env`] returns
/// `Err` / [`crate::from_env`] returns `None`.
pub fn disabled_reason() -> Disabled {
    if !is_enabled() {
        Disabled::FeatureDisabled
    } else {
        Disabled::MissingKey
    }
}

/// Resolve the cache directory, reusing organize's convention: `SEARCH_LLM_CACHE`, else
/// `SEARCH_DATA_DIR/llm-cache`, else `/var/lib/search/llm-cache`.
fn resolve_cache_dir() -> PathBuf {
    if let Some(dir) = env_trim("SEARCH_LLM_CACHE") {
        return PathBuf::from(dir);
    }
    let data = env_trim("SEARCH_DATA_DIR").unwrap_or_else(|| "/var/lib/search".to_string());
    PathBuf::from(data).join("llm-cache")
}

/// A model id that spends part of its token budget on a hidden reasoning trace.
fn is_reasoning_model(model: &str) -> bool {
    let m = model.to_ascii_lowercase();
    m.contains("thinking") || m.contains("reason") || m.contains("r1") || m.contains("o1")
}

impl AnswerConfig {
    /// Build the configuration from the environment.
    ///
    /// Each vendor-neutral `SEARCH_LLM_*` variable wins when set; the vendor-specific
    /// `SEARCH_ANSWER_*` (and `KITANI_API_KEY`) variable is the fallback, so existing deployments
    /// keep working unchanged.
    ///
    /// Returns `Err(Disabled::FeatureDisabled)` when `SEARCH_ANSWER_ENABLED` is unset/0 and
    /// `Err(Disabled::MissingKey)` when the feature is on but neither key is set.
    pub fn from_env() -> Result<AnswerConfig, Disabled> {
        if !is_enabled() {
            return Err(Disabled::FeatureDisabled);
        }
        if api_key().is_none() {
            return Err(Disabled::MissingKey);
        }

        let base_url = env_trim("SEARCH_LLM_BASE_URL")
            .or_else(|| env_trim("SEARCH_ANSWER_BASE_URL"))
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_string())
            .trim_end_matches('/')
            .to_string();
        let model = env_trim("SEARCH_LLM_MODEL")
            .or_else(|| env_trim("SEARCH_ANSWER_MODEL"))
            .unwrap_or_else(|| DEFAULT_MODEL.to_string());

        let mut timeout_ms = env_trim("SEARCH_ANSWER_TIMEOUT_MS")
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(DEFAULT_TIMEOUT_MS);
        let mut max_tokens = env_trim("SEARCH_ANSWER_MAX_TOKENS")
            .and_then(|v| v.parse::<u32>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(DEFAULT_MAX_TOKENS);

        // A reasoning model needs room for its hidden trace and a long timeout; without this it
        // returns `finish_reason=length` with null content. This is an explicit opt-in to a
        // slower call, never the default.
        if is_reasoning_model(&model) {
            max_tokens = max_tokens.max(REASONING_MIN_TOKENS);
            timeout_ms = timeout_ms.max(REASONING_TIMEOUT_MS);
        }

        let concurrency = env_trim("SEARCH_ANSWER_CONCURRENCY")
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(DEFAULT_CONCURRENCY);

        Ok(AnswerConfig {
            base_url,
            model,
            timeout_ms,
            max_tokens,
            concurrency,
            cache_dir: resolve_cache_dir(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // Environment variables are process-global; serialize the tests that mutate them.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    const VARS: [&str; 7] = [
        "SEARCH_ANSWER_ENABLED",
        "SEARCH_LLM_BASE_URL",
        "SEARCH_LLM_API_KEY",
        "SEARCH_LLM_MODEL",
        "SEARCH_ANSWER_BASE_URL",
        "SEARCH_ANSWER_MODEL",
        "KITANI_API_KEY",
    ];

    /// Clear every variable `from_env` reads and run `f`; restore afterwards even on panic.
    fn with_clean_env<F: FnOnce()>(f: F) {
        let saved: Vec<(&str, Option<String>)> =
            VARS.iter().map(|n| (*n, std::env::var(n).ok())).collect();
        for n in VARS {
            std::env::remove_var(n);
        }
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
        for (n, v) in saved {
            match v {
                Some(v) => std::env::set_var(n, v),
                None => std::env::remove_var(n),
            }
        }
        if let Err(e) = result {
            std::panic::resume_unwind(e);
        }
    }

    #[test]
    fn search_llm_vars_take_precedence_over_answer_specific() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        with_clean_env(|| {
            std::env::set_var("SEARCH_ANSWER_ENABLED", "1");
            std::env::set_var("SEARCH_LLM_BASE_URL", "https://neutral.example/v1");
            std::env::set_var("SEARCH_LLM_API_KEY", "neutral-key");
            std::env::set_var("SEARCH_LLM_MODEL", "neutral-model");
            std::env::set_var("SEARCH_ANSWER_BASE_URL", "https://kitani.example/v1");
            std::env::set_var("SEARCH_ANSWER_MODEL", "kitani-model");
            std::env::set_var("KITANI_API_KEY", "kitani-key");

            let cfg = AnswerConfig::from_env().expect("neutral config");
            assert_eq!(cfg.base_url, "https://neutral.example/v1");
            assert_eq!(cfg.model, "neutral-model");
            assert_eq!(api_key().as_deref(), Some("neutral-key"));
        });
    }

    #[test]
    fn answer_and_kitani_vars_remain_the_fallback() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        with_clean_env(|| {
            std::env::set_var("SEARCH_ANSWER_ENABLED", "1");
            std::env::set_var("SEARCH_ANSWER_BASE_URL", "https://kitani.example/v1/");
            std::env::set_var("SEARCH_ANSWER_MODEL", "kitani-model");
            std::env::set_var("KITANI_API_KEY", "kitani-key");

            let cfg = AnswerConfig::from_env().expect("fallback config");
            assert_eq!(
                cfg.base_url, "https://kitani.example/v1",
                "trailing slash trimmed"
            );
            assert_eq!(cfg.model, "kitani-model");
            assert_eq!(api_key().as_deref(), Some("kitani-key"));
        });
    }

    #[test]
    fn neither_key_disables_with_missing_key() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        with_clean_env(|| {
            std::env::set_var("SEARCH_ANSWER_ENABLED", "1");
            assert!(AnswerConfig::from_env().is_err());
            assert_eq!(disabled_reason(), Disabled::MissingKey);
        });
    }

    #[test]
    fn feature_disabled_is_reported_first() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        with_clean_env(|| {
            assert_eq!(
                AnswerConfig::from_env().expect_err("off by default"),
                Disabled::FeatureDisabled
            );
        });
    }
}
