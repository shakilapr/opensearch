//! LLM configuration for the organizer.
//!
//! The provider is OpenAI-chat-compatible. Base URL and credentials come from the
//! environment; a credential is **never** hardcoded here. The vendor-neutral `SEARCH_LLM_*`
//! variables take precedence over the vendor-specific `CROWBOT_*` ones, which remain the
//! fallback. `from_env` fails loudly (naming `CROWBOT_API_KEY`) when neither key is present so a
//! misconfigured process cannot silently run against the wrong (or no) endpoint.

use crate::error::OrganizeError;

/// Default base URL for the OpenAI-chat-compatible provider.
pub const DEFAULT_BASE_URL: &str = "https://api.crowbot.sh/v1";
/// Default primary model id.
pub const DEFAULT_MODEL: &str = "deepseek-v4.1-flash";
/// Default escalation model id: Sonar, a *different* model.
///
/// Escalation exists to ask a different model when the primary's answer is unusable, so this must
/// not name the primary — two identical ids would make every escalation a silent retry of the same
/// model at the same cost. The deployment sets `SEARCH_LLM_ESCALATE_MODEL` explicitly.
pub const DEFAULT_ESCALATE_MODEL: &str = "t/sonar";
/// Default per-request timeout in seconds.
///
/// Generous on purpose. The answer path is latency-sensitive and bounded separately
/// (`SEARCH_ANSWER_TIMEOUT_MS`); this one covers batch structuring, where a long page and a
/// multi-model fallback chain can each add seconds, and a short timeout would turn a slow success
/// into a hard failure measured at ~3 s per call against the current providers.
pub const DEFAULT_TIMEOUT_S: u64 = 180;

/// Default base URL of the **fallback** provider (the apizio relay in front of Perplexity Sonar),
/// tried only when a primary call fails.
pub const DEFAULT_FALLBACK_BASE_URL: &str = "https://newapi.apizio.com/v1";
/// Default **fallback** model id: `t/sonar`, the search-specialised model.
pub const DEFAULT_FALLBACK_MODEL: &str = "t/sonar";

fn env_trim(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// The `User-Agent` sent with every completion request.
///
/// Cosmetic for the providers in use — crowbot and the apizio relay both answer with and without
/// one (measured 2026-10-13: accepted either way, ~3 s) — but kept because a Cloudflare-fronted
/// relay can reject a request carrying no `User-Agent` outright: the former cun.ai fallback
/// answered HTTP 403 (`error code: 1010`) until one was sent. The header costs nothing.
pub const DEFAULT_LLM_USER_AGENT: &str = "search-engine/0.1 (+https://github.com/agent-native-search)";

/// The `User-Agent` for LLM requests: `SEARCH_LLM_USER_AGENT`, else [`DEFAULT_LLM_USER_AGENT`].
pub fn llm_user_agent() -> String {
    env_trim("SEARCH_LLM_USER_AGENT").unwrap_or_else(|| DEFAULT_LLM_USER_AGENT.to_string())
}

/// The ordered model list for the planner: `SEARCH_LLM_MODELS` (comma-separated), else empty (the
/// caller then uses the single configured model).
///
/// Free models first: a model that fails moves to the next entry rather than losing the plan.
pub fn model_chain() -> Vec<String> {
    env_trim("SEARCH_LLM_MODELS")
        .map(|listed| {
            listed
                .split(',')
                .map(|m| m.trim().to_string())
                .filter(|m| !m.is_empty())
                .collect()
        })
        .unwrap_or_default()
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

/// The fallback credential: `SEARCH_LLM_FALLBACK_API_KEY`, else `CROWBOT_API_KEY`.
///
/// `None` **disables** the fallback entirely — the primary provider is then the only one tried.
/// A credential is read from the environment only, never from a file, never logged.
pub fn fallback_api_key() -> Option<String> {
    env_trim("SEARCH_LLM_FALLBACK_API_KEY").or_else(|| env_trim("CROWBOT_API_KEY"))
}

/// Resolved LLM configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlmConfig {
    /// Base URL of the OpenAI-chat-compatible endpoint (no trailing `/chat/completions`).
    pub base_url: String,
    /// Primary (cheap) model id.
    pub model: String,
    /// The ordered model chain for **this** config's provider, empty when the caller supplies a
    /// single model.
    ///
    /// Resolved by whoever builds the config, not read from the environment by the client: a
    /// caller pointing at a different vendor (the planner with `SEARCH_PLANNER_BASE_URL`) must not
    /// ask that vendor for another provider's model ids — doing so returns a 404 per model and
    /// loses the plan.
    pub models: Vec<String>,
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
    /// Each vendor-neutral `SEARCH_LLM_*` variable wins when set; the vendor-specific `CROWBOT_*`
    /// variable is the fallback, so existing deployments keep working unchanged.
    ///
    /// * `SEARCH_LLM_BASE_URL` / `CROWBOT_BASE_URL` — optional, defaults to [`DEFAULT_BASE_URL`].
    /// * `SEARCH_LLM_API_KEY` / `CROWBOT_API_KEY` — **required**; an error naming `CROWBOT_API_KEY`
    ///   is returned when both are absent/empty. Read from the environment only.
    /// * `SEARCH_LLM_MODEL` / `CROWBOT_MODEL` — optional primary model id.
    /// * `CROWBOT_ESCALATE_MODEL` — optional escalation model id (no neutral override).
    /// * `CROWBOT_TIMEOUT_S` — optional per-request timeout in seconds.
    pub fn from_env() -> Result<Self, OrganizeError> {
        let base_url = std::env::var("SEARCH_LLM_BASE_URL")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .or_else(|| {
                std::env::var("CROWBOT_BASE_URL")
                    .ok()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
            })
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());

        let api_key = match std::env::var("SEARCH_LLM_API_KEY") {
            Ok(v) if !v.trim().is_empty() => v,
            _ => match std::env::var("OPENROUTER_API_KEY") {
                Ok(v) if !v.trim().is_empty() => v,
                _ => match std::env::var("CROWBOT_API_KEY") {
                    Ok(v) if !v.trim().is_empty() => v,
                    _ => {
                        return Err(OrganizeError::MissingEnv(
                            "CROWBOT_API_KEY (or SEARCH_LLM_API_KEY / OPENROUTER_API_KEY)".to_string(),
                        ))
                    }
                },
            },
        };

        let model = std::env::var("SEARCH_LLM_MODEL")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .or_else(|| {
                std::env::var("CROWBOT_MODEL")
                    .ok()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
            })
            .unwrap_or_else(|| DEFAULT_MODEL.to_string());

        let escalate_model = std::env::var("SEARCH_LLM_ESCALATE_MODEL")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .or_else(|| {
                std::env::var("CROWBOT_ESCALATE_MODEL")
                    .ok()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
            })
            .unwrap_or_else(|| DEFAULT_ESCALATE_MODEL.to_string());

        let timeout_s = std::env::var("CROWBOT_TIMEOUT_S")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(DEFAULT_TIMEOUT_S);

        Ok(LlmConfig {
            base_url,
            model,
            models: model_chain(),
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

    const VARS: [&str; 9] = [
        "SEARCH_LLM_BASE_URL",
        "SEARCH_LLM_API_KEY",
        "SEARCH_LLM_MODEL",
        "SEARCH_LLM_ESCALATE_MODEL",
        "OPENROUTER_API_KEY",
        "CROWBOT_BASE_URL",
        "CROWBOT_API_KEY",
        "CROWBOT_MODEL",
        "CROWBOT_ESCALATE_MODEL",
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
    fn search_llm_vars_take_precedence_over_crowbot() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        with_clean_env(|| {
            std::env::set_var("SEARCH_LLM_BASE_URL", "https://neutral.example/v1");
            std::env::set_var("SEARCH_LLM_API_KEY", "neutral-key");
            std::env::set_var("SEARCH_LLM_MODEL", "neutral-model");
            std::env::set_var("CROWBOT_BASE_URL", "https://crowbot.example/v1");
            std::env::set_var("CROWBOT_API_KEY", "crowbot-key");
            std::env::set_var("CROWBOT_MODEL", "crowbot-model");

            let cfg = LlmConfig::from_env().expect("neutral config");
            assert_eq!(cfg.base_url, "https://neutral.example/v1");
            assert_eq!(cfg.model, "neutral-model");
            assert_eq!(cfg.api_key.as_deref(), Some("neutral-key"));
        });
    }

    #[test]
    fn crowbot_vars_remain_the_fallback() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        with_clean_env(|| {
            std::env::set_var("CROWBOT_BASE_URL", "https://crowbot.example/v1");
            std::env::set_var("CROWBOT_API_KEY", "crowbot-key");
            std::env::set_var("CROWBOT_MODEL", "crowbot-model");

            let cfg = LlmConfig::from_env().expect("crowbot config");
            assert_eq!(cfg.base_url, "https://crowbot.example/v1");
            assert_eq!(cfg.model, "crowbot-model");
            assert_eq!(cfg.api_key.as_deref(), Some("crowbot-key"));
        });
    }

    #[test]
    fn precedence_is_per_variable() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        with_clean_env(|| {
            std::env::set_var("SEARCH_LLM_BASE_URL", "https://neutral.example/v1");
            std::env::set_var("CROWBOT_API_KEY", "crowbot-key");
            std::env::set_var("CROWBOT_MODEL", "crowbot-model");

            let cfg = LlmConfig::from_env().expect("mixed config");
            assert_eq!(cfg.base_url, "https://neutral.example/v1");
            assert_eq!(cfg.model, "crowbot-model", "model falls back independently");
            assert_eq!(cfg.api_key.as_deref(), Some("crowbot-key"));
        });
    }

    #[test]
    fn no_key_reports_every_accepted_source() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        with_clean_env(|| {
            let err = LlmConfig::from_env().expect_err("key is required");
            // The message must name the sources, so an operator with any one of them set knows why
            // the process refused to start.
            assert!(matches!(err, OrganizeError::MissingEnv(_)));
            let msg = err.to_string();
            assert!(msg.contains("SEARCH_LLM_API_KEY"), "{msg}");
            assert!(msg.contains("OPENROUTER_API_KEY"), "{msg}");
            assert!(msg.contains("CROWBOT_API_KEY"), "{msg}");
        });
    }

    /// The OpenRouter key is accepted on its own (crowbot is dead; OpenRouter is the search path's
    /// provider), and the neutral `SEARCH_LLM_*` names still win when both are present.
    #[test]
    fn openrouter_key_is_accepted() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        with_clean_env(|| {
            std::env::set_var("OPENROUTER_API_KEY", "or-key");
            std::env::set_var("SEARCH_LLM_BASE_URL", "https://openrouter.ai/api/v1");
            std::env::set_var("SEARCH_LLM_MODEL", "google/gemma-4-31b-it:free");

            let cfg = LlmConfig::from_env().expect("openrouter config");
            assert_eq!(cfg.api_key.as_deref(), Some("or-key"));
            assert_eq!(cfg.model, "google/gemma-4-31b-it:free");
        });
    }

    /// The escalate model has a vendor-neutral override, so an OpenRouter deployment is not stuck
    /// with crowbot's thinking-model id.
    #[test]
    fn neutral_escalate_model_override() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        with_clean_env(|| {
            std::env::set_var("CROWBOT_API_KEY", "k");
            std::env::set_var("SEARCH_LLM_ESCALATE_MODEL", "google/gemma-4-31b-it:free");

            let cfg = LlmConfig::from_env().expect("config");
            assert_eq!(cfg.escalate_model, "google/gemma-4-31b-it:free");
        });
    }
}
