//! LLM configuration for the organizer.
//!
//! The provider is OpenAI-chat-compatible. Base URL and credentials come from the
//! environment; a credential is **never** hardcoded here. `from_env` fails loudly (naming the
//! variable) when `KITANI_API_KEY` is absent so a misconfigured process cannot silently run
//! against the wrong (or no) endpoint.

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
    /// * `KITANI_BASE_URL` — optional, defaults to [`DEFAULT_BASE_URL`].
    /// * `KITANI_API_KEY`  — **required**; an error naming it is returned when absent/empty.
    /// * `KITANI_MODEL` / `KITANI_ESCALATE_MODEL` — optional model ids.
    /// * `KITANI_TIMEOUT_S` — optional per-request timeout in seconds.
    pub fn from_env() -> Result<Self, OrganizeError> {
        let base_url = std::env::var("KITANI_BASE_URL")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());

        let api_key = match std::env::var("KITANI_API_KEY") {
            Ok(v) if !v.trim().is_empty() => v,
            _ => return Err(OrganizeError::MissingEnv("KITANI_API_KEY".to_string())),
        };

        let model = std::env::var("KITANI_MODEL")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
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
