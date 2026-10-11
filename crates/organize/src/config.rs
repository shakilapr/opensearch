//! LLM configuration for the organizer.
//!
//! The provider is OpenAI-chat-compatible. Base URL and credentials come from the
//! environment; a credential is **never** hardcoded here. The vendor-neutral `SEARCH_LLM_*`
//! variables take precedence over the vendor-specific `KITANI_*` ones, which remain the
//! fallback. `from_env` fails loudly (naming `KITANI_API_KEY`) when neither key is present so a
//! misconfigured process cannot silently run against the wrong (or no) endpoint.

use crate::error::OrganizeError;

/// Default base URL for the OpenAI-chat-compatible provider.
pub const DEFAULT_BASE_URL: &str = "https://api.kitani.ai/v1";
/// Default primary (cheap) model id.
pub const DEFAULT_MODEL: &str = "deepseek/deepseek-v4.1-flash";
/// Default escalation (thinking) model id.
pub const DEFAULT_ESCALATE_MODEL: &str = "deepseek/deepseek-v4.1-flash-thinking";
/// Default per-request timeout in seconds.
///
/// The cheap model is *slow* on long pages: a 12k-character prompt was measured taking ~48 s,
/// and the thinking model takes longer still. A short timeout turns those into hard failures.
pub const DEFAULT_TIMEOUT_S: u64 = 180;

/// Resolved LLM configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlmConfig {
    /// Base URL of the OpenAI-chat-compatible endpoint (no trailing `/chat/completions`).
    pub base_url: String,
    /// Primary (cheap) model id.
    pub model: String,
    /// Escalation (thinking) model id, used once when the primary output fails validation.
    pub escalate_model: String,
    /// API key; `Some` for the real path, `None` only in hermetic tests.
    pub api_key: Option<String>,
    /// Per-request HTTP timeout in seconds.
    pub timeout_s: u64,
}

impl LlmConfig {
    /// Build a configuration from the environment.
    ///
    /// Each vendor-neutral `SEARCH_LLM_*` variable wins when set; the vendor-specific `KITANI_*`
    /// variable is the fallback, so existing deployments keep working unchanged.
    ///
    /// * `SEARCH_LLM_BASE_URL` / `KITANI_BASE_URL` — optional, defaults to [`DEFAULT_BASE_URL`].
    /// * `SEARCH_LLM_API_KEY` / `KITANI_API_KEY` — **required**; an error naming `KITANI_API_KEY`
    ///   is returned when both are absent/empty. Read from the environment only.
    /// * `SEARCH_LLM_MODEL` / `KITANI_MODEL` — optional primary model id.
    /// * `KITANI_ESCALATE_MODEL` — optional escalation model id (no neutral override).
    /// * `KITANI_TIMEOUT_S` — optional per-request timeout in seconds.
    pub fn from_env() -> Result<Self, OrganizeError> {
        let base_url = std::env::var("SEARCH_LLM_BASE_URL")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .or_else(|| {
                std::env::var("KITANI_BASE_URL")
                    .ok()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
            })
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());

        let api_key = match std::env::var("SEARCH_LLM_API_KEY") {
            Ok(v) if !v.trim().is_empty() => v,
            _ => match std::env::var("KITANI_API_KEY") {
                Ok(v) if !v.trim().is_empty() => v,
                _ => return Err(OrganizeError::MissingEnv("KITANI_API_KEY".to_string())),
            },
        };

        let model = std::env::var("SEARCH_LLM_MODEL")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .or_else(|| {
                std::env::var("KITANI_MODEL")
                    .ok()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
            })
            .unwrap_or_else(|| DEFAULT_MODEL.to_string());

        let escalate_model = std::env::var("KITANI_ESCALATE_MODEL")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_ESCALATE_MODEL.to_string());

        let timeout_s = std::env::var("KITANI_TIMEOUT_S")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(DEFAULT_TIMEOUT_S);

        Ok(LlmConfig {
            base_url,
            model,
            escalate_model,
            api_key: Some(api_key),
            timeout_s,
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
        "SEARCH_LLM_BASE_URL",
        "SEARCH_LLM_API_KEY",
        "SEARCH_LLM_MODEL",
        "KITANI_BASE_URL",
        "KITANI_API_KEY",
        "KITANI_MODEL",
        "KITANI_ESCALATE_MODEL",
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
    fn search_llm_vars_take_precedence_over_kitani() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        with_clean_env(|| {
            std::env::set_var("SEARCH_LLM_BASE_URL", "https://neutral.example/v1");
            std::env::set_var("SEARCH_LLM_API_KEY", "neutral-key");
            std::env::set_var("SEARCH_LLM_MODEL", "neutral-model");
            std::env::set_var("KITANI_BASE_URL", "https://kitani.example/v1");
            std::env::set_var("KITANI_API_KEY", "kitani-key");
            std::env::set_var("KITANI_MODEL", "kitani-model");

            let cfg = LlmConfig::from_env().expect("neutral config");
            assert_eq!(cfg.base_url, "https://neutral.example/v1");
            assert_eq!(cfg.model, "neutral-model");
            assert_eq!(cfg.api_key.as_deref(), Some("neutral-key"));
        });
    }

    #[test]
    fn kitani_vars_remain_the_fallback() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        with_clean_env(|| {
            std::env::set_var("KITANI_BASE_URL", "https://kitani.example/v1");
            std::env::set_var("KITANI_API_KEY", "kitani-key");
            std::env::set_var("KITANI_MODEL", "kitani-model");

            let cfg = LlmConfig::from_env().expect("kitani config");
            assert_eq!(cfg.base_url, "https://kitani.example/v1");
            assert_eq!(cfg.model, "kitani-model");
            assert_eq!(cfg.api_key.as_deref(), Some("kitani-key"));
        });
    }

    #[test]
    fn precedence_is_per_variable() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        with_clean_env(|| {
            std::env::set_var("SEARCH_LLM_BASE_URL", "https://neutral.example/v1");
            std::env::set_var("KITANI_API_KEY", "kitani-key");
            std::env::set_var("KITANI_MODEL", "kitani-model");

            let cfg = LlmConfig::from_env().expect("mixed config");
            assert_eq!(cfg.base_url, "https://neutral.example/v1");
            assert_eq!(cfg.model, "kitani-model", "model falls back independently");
            assert_eq!(cfg.api_key.as_deref(), Some("kitani-key"));
        });
    }

    #[test]
    fn neither_key_keeps_the_existing_error() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        with_clean_env(|| {
            let err = LlmConfig::from_env().expect_err("key is required");
            assert!(matches!(err, OrganizeError::MissingEnv(ref v) if v == "KITANI_API_KEY"));
            assert!(err.to_string().contains("KITANI_API_KEY"));
        });
    }
}
